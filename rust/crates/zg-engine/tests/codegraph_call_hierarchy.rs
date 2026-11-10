//! Source → production indexing → callers/callees acceptance cases.
//! Case layout follows walknode-onto-main/tests/call_hierarchy.rs.
//! Expectations are literal assertions, never captured query output.
//! Only embeddings are stubbed; no entities or graph edges are seeded.
//!
//! Upstream: colbymchenry/codegraph, dc4ccf6b1468bb77eed3e765f3200ae16a6834bd.
//! Each fixture cites its original assertions. Identical source trees share one
//! Case; their distinct queries are retained. Parameterized source variants stay
//! separate. Order is ignored, but definition identity and multiplicity are not.
//!
//! Scope: supported-language caller/callee results from source-backed tests.
//! Excludes generic smoke checks, HTTP-route graphs, other languages and tests
//! requiring seeded graph rows or `CodeGraph`'s private fault-injection hooks.
//! Confidence/provenance-only assertions cannot be represented by this API.
//! Assertions incompatible with zg's Calls-only, per-definition, per-call-site
//! response contract are excluded; genuine missing and incorrect edges remain.
//! These are engine acceptance tests, not CLI/MCP text-format compatibility tests.

#![recursion_limit = "512"]

#[allow(dead_code)]
mod support;

use serde_json::{Value, json};
use std::{fs, path::Path};
use support::{EmbeddingServer, configure_remote_model, index_options};
use zg_engine::{ZvecGrep, api::relationships::RelationshipOptions};

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

#[derive(Default)]
struct Query {
    operation: &'static str,
    symbol: &'static str,
    expected: Value,
    // Keep upstream toContain/not.toContain semantics where its list is partial.
    check: Check,
    file: Option<&'static str>,
    endpoint_suffix: Option<&'static str>,
    endpoint_prefix: Option<&'static str>,
    edge_filters: Vec<(&'static str, &'static str, bool)>,
    limit: Option<usize>,
    definition_line: Option<usize>,
    short_names: bool,
    // Used only where the upstream assertion explicitly constructs a Set.
    distinct_symbols: bool,
    // An upstream negative assertion may apply to every same-named definition.
    any_definition: bool,
}

#[derive(Default, Clone, Copy)]
enum Check {
    #[default]
    Exact,
    Contains,
    Excludes,
}

// Compare only declared fields, independently of response order. Consume each
// matching item once so duplicate/missing relationships still fail exact checks.
fn matches_expected(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected.iter().all(|(key, wanted)| {
            if key == "column" {
                return true;
            }
            let Some(value) = actual.get(key) else {
                return false;
            };
            if matches!(key.as_str(), "line" | "startLine")
                && let Some(choices) = wanted.as_array()
            {
                return choices.contains(value);
            }
            matches_expected(value, wanted)
        }),
        (Value::Array(actual), Value::Array(expected)) => {
            actual.len() == expected.len() && contains_expected(actual, expected)
        }
        _ => actual == expected,
    }
}

fn contains_expected(actual: &[Value], expected: &[Value]) -> bool {
    let mut consumed = vec![false; actual.len()];
    for wanted in expected {
        let Some(index) = actual
            .iter()
            .enumerate()
            .position(|(index, value)| !consumed[index] && matches_expected(value, wanted))
        else {
            return false;
        };
        consumed[index] = true;
    }
    true
}

fn matches_query(actual: &Value, query: &Query) -> bool {
    if matches!(query.check, Check::Exact) {
        return matches_expected(actual, &query.expected);
    }
    let actual = actual.as_array().expect("actual definition groups");
    let expected = query
        .expected
        .as_array()
        .expect("expected definition groups");
    let edges_match = |group: &Value, wanted: &Value| {
        let edges = group["edges"].as_array().expect("actual edges");
        let wanted = wanted["edges"].as_array().expect("expected edges");
        match query.check {
            Check::Contains => contains_expected(edges, wanted),
            Check::Excludes => !edges
                .iter()
                .any(|edge| wanted.iter().any(|w| matches_expected(edge, w))),
            Check::Exact => unreachable!(),
        }
    };
    if query.any_definition {
        return actual.iter().all(|group| edges_match(group, &expected[0]));
    }
    actual.len() == expected.len()
        && expected.iter().all(|wanted| {
            let mut root = wanted.clone();
            root.as_object_mut().expect("definition").remove("edges");
            actual
                .iter()
                .any(|group| matches_expected(group, &root) && edges_match(group, wanted))
        })
}

async fn check_query(
    engine: &ZvecGrep,
    root: &Path,
    case: &Case,
    query: &Query,
) -> Result<(), String> {
    let options = RelationshipOptions {
        root: Some(root.to_path_buf()),
        symbol: query.symbol.into(),
        limit: Some(query.limit.unwrap_or(50)),
        globs: query.file.into_iter().map(Into::into).collect(),
        ..RelationshipOptions::default()
    };
    let result = match query.operation {
        "callers" => engine.callers(options).await,
        "callees" => engine.callees(options).await,
        _ => panic!("unknown query operation"),
    }
    .map_err(|error| {
        format!(
            "{}: {} {}: {error}",
            case.name, query.operation, query.symbol
        )
    })?;
    if !query
        .expected
        .as_array()
        .is_some_and(|groups| groups.iter().any(|group| group.get("truncated").is_some()))
        && result.matches.iter().any(|group| group.truncated)
    {
        return Err(format!(
            "{}: unexpectedly truncated {}",
            case.name, query.symbol
        ));
    }
    let mut actual = serde_json::to_value(result.matches).expect("serialize matches");
    if let Some(line) = query.definition_line {
        actual
            .as_array_mut()
            .expect("definition groups")
            .retain(|group| group["startLine"] == line);
    }
    for group in actual.as_array_mut().expect("definition groups") {
        let edges = group["edges"].as_array_mut().expect("edges");
        if let Some(suffix) = query.endpoint_suffix {
            edges.retain(|edge| {
                edge["symbol"]["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with(suffix))
            });
        }
        if let Some(prefix) = query.endpoint_prefix {
            edges.retain(|edge| {
                edge["symbol"]["name"]
                    .as_str()
                    .is_some_and(|name| name.starts_with(prefix))
            });
        }
        for (operation, pattern, negate) in &query.edge_filters {
            edges.retain(|edge| {
                let name = edge["symbol"]["name"].as_str().unwrap_or("");
                let path = edge["symbol"]["filePath"].as_str().unwrap_or("");
                let matched = match *operation {
                    "includes" => name.contains(pattern) || path.contains(pattern),
                    "startsWith" => name.starts_with(pattern),
                    "endsWith" => name.ends_with(pattern),
                    _ => panic!("unknown upstream filter"),
                };
                matched != *negate
            });
        }
        if query.short_names {
            for edge in edges.iter_mut() {
                if let Some(name) = edge["symbol"]["name"].as_str() {
                    edge["symbol"]["name"] = json!(name.rsplit("::").next().expect("symbol name"));
                }
            }
        }
        if query.distinct_symbols {
            edges.sort_by_cached_key(|edge| edge["symbol"].to_string());
            edges.dedup_by(|a, b| a["symbol"] == b["symbol"]);
        }
    }
    if !matches_query(&actual, query) {
        return Err(format!(
            "{}: {} {}\nexpected: {}\nactual: {}",
            case.name, query.operation, query.symbol, query.expected, actual
        ));
    }
    Ok(())
}
async fn check_queries(engine: &ZvecGrep, root: &Path, case: &Case) -> Result<(), String> {
    let mut errors = Vec::new();
    for query in &case.queries {
        if let Err(error) = check_query(engine, root, case, query).await {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n\n"))
    }
}

async fn check_case(case: Case) {
    check_stages(&[case], false).await;
}

async fn check_stages(stages: &[Case], rebuild_each: bool) {
    let _permit = WORKSPACES.acquire().await.expect("workspace permit");
    let directory = tempfile::tempdir().expect("source workspace");
    let root = directory.path().canonicalize().expect("canonical root");
    let server = EmbeddingServer::start().expect("local embedding server");
    configure_remote_model(&root, server.address).expect("embedding configuration");
    for (stage, case) in stages.iter().enumerate() {
        if stage > 0 {
            for previous in &stages[stage - 1].files {
                if !case.files.iter().any(|file| file.path == previous.path) {
                    fs::remove_file(root.join(previous.path)).expect("remove source");
                }
            }
        }
        for file in &case.files {
            let path = root.join(file.path);
            fs::create_dir_all(path.parent().expect("source parent")).expect("source directory");
            fs::write(path, file.source).expect("write source");
        }
        for rebuild in if rebuild_each {
            &[false, true][..]
        } else {
            &[false][..]
        } {
            let engine = ZvecGrep::new();
            let mut options = index_options(&root);
            options.graph_indexing = Some(true);
            options.rebuild = *rebuild || stage == 0;
            let indexed = engine.index(options).await;
            engine.close();
            let indexed = indexed.expect("index sources");
            assert_eq!(indexed.files_failed, 0, "{}: {indexed:?}", case.name);
            // Reopen to exercise the persisted graph, as the previous tests did.
            let engine = ZvecGrep::new();
            let result = check_queries(&engine, &root, case).await;
            engine.close();
            result.unwrap_or_else(|error| panic!("{error}"));
        }
    }
}

#[test]
fn expected_fields_preserve_identity_counts_and_line_choices() {
    let expected = json!([{"name":"caller","filePath":"a.cc","startLine":1,"edges":[
        {"symbol":{"name":"work","filePath":"a.cc","startLine":2},"line":[7,8]}
    ]}]);
    let mut actual = expected.clone();
    actual[0]["edges"][0]["line"] = json!(8);
    assert!(matches_expected(&actual, &expected));
    actual[0]["edges"][0]["line"] = json!(9);
    assert!(!matches_expected(&actual, &expected));
    actual[0]["edges"][0]["line"] = json!(7);
    actual[0]["edges"][0]["symbol"]["filePath"] = json!("b.cc");
    assert!(!matches_expected(&actual, &expected));
    actual[0]["edges"][0]["symbol"]["filePath"] = json!("a.cc");
    let duplicate = actual[0]["edges"][0].clone();
    actual[0]["edges"]
        .as_array_mut()
        .expect("edges")
        .push(duplicate);
    assert!(!matches_expected(&actual, &expected));
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-iterator-arrow.test.ts:48 — reproduction: rocksdb's `auto it = logs_.begin()
// it->IsSyncing()`
#[tokio::test]
async fn cpp_auto_iterator_arrow_case_48_reproduction_rocksdb_s_auto_it_logs_begin_it_issyncing() {
    check_case(Case {
        name: "cpp_auto_iterator_arrow_case_48_reproduction_rocksdb_s_auto_it_logs_begin_it_issyncing",
        files: vec![
            SourceFile {
                path: "db/log_writer.h",
                source: "class Writer {\n public:\n  bool IsSyncing() const { return false; }\n};\n",
            },
            SourceFile {
                path: "db/db_impl.h",
                source: "#include <deque>\nclass DBImpl {\n public:\n  struct LogWriterNumber {\n    bool IsSyncing() const { return true; }\n    void ReleaseWriter() {}\n  };\n  void MarkLogsSynced();\n private:\n  std::deque<LogWriterNumber> logs_;\n};\n",
            },
            SourceFile {
                path: "db/db_impl.cc",
                source: "#include \"db/db_impl.h\"\n#include \"db/log_writer.h\"\nvoid DBImpl::MarkLogsSynced() {\n  auto it = logs_.begin();\n  if (it->IsSyncing()) it->ReleaseWriter();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "DBImpl::MarkLogsSynced",
                expected: json!([
                    {"name": "DBImpl::MarkLogsSynced", "edges": [
                        {"symbol": {"name": "DBImpl::LogWriterNumber::IsSyncing"}},
                        {"symbol": {"name": "DBImpl::LogWriterNumber::ReleaseWriter"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-iterator-arrow.test.ts:85 — from a standard search, and a reverse iterator
#[tokio::test]
async fn cpp_auto_iterator_arrow_case_85_from_a_standard_search_and_a_reverse_iterator() {
    check_case(Case {
        name: "cpp_auto_iterator_arrow_case_85_from_a_standard_search_and_a_reverse_iterator",
        files: vec![
            SourceFile {
                path: "util/entries.h",
                source: "class Entry {\n public:\n  const char* name() const { return nullptr; }\n};\nclass Column {\n public:\n  const char* name() const { return nullptr; }\n};\n",
            },
            SourceFile {
                path: "util/lookup.cc",
                source: "#include <algorithm>\n#include <vector>\n#include \"util/entries.h\"\nconst char* Find(const std::vector<Entry>& entries, const Entry& key) {\n  auto it = std::lower_bound(entries.begin(), entries.end(), key);\n  return it->name();\n}\nconst char* Last(std::vector<Entry>& entries) {\n  auto it = entries.rbegin();\n  return it->name();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Find",
                expected: json!([
                    {"name": "Find", "edges": [
                        {"symbol": {"name": "Entry::name"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Last",
                expected: json!([
                    {"name": "Last", "edges": [
                        {"symbol": {"name": "Entry::name"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-iterator-arrow.test.ts:119 — the element is looked up where the container's type is written
#[tokio::test]
async fn cpp_auto_iterator_arrow_case_119_the_element_is_looked_up_where_the_container_s_type_is_written()
 {
    check_case(Case {
        name: "cpp_auto_iterator_arrow_case_119_the_element_is_looked_up_where_the_container_s_type_is_written",
        files: vec![
            SourceFile {
                path: "db/version_set.h",
                source: "#include <vector>\nstruct ObsoleteFile {\n  void DeleteFile() {}\n};\nclass VersionSet {\n public:\n  struct ObsoleteFile {\n    void DeleteFile() {}\n  };\n  std::vector<ObsoleteFile> obsolete_files_;\n};\n",
            },
            SourceFile {
                path: "db/purge.cc",
                source: "#include \"db/version_set.h\"\nvoid Purge(VersionSet* vs) {\n  auto it = vs->obsolete_files_.begin();\n  it->DeleteFile();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Purge",
                expected: json!([
                    {"name": "Purge", "edges": [
                        {"symbol": {"name": "VersionSet::ObsoleteFile::DeleteFile"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-iterator-arrow.test.ts:149 — a smart pointer member's type is still looked up where the call is: a pimpl defined in the source file
#[tokio::test]
async fn cpp_auto_iterator_arrow_case_149_a_smart_pointer_member_s_type_is_still_looked_up_where_the_call_is_a_pimpl()
 {
    check_case(Case {
        name: "cpp_auto_iterator_arrow_case_149_a_smart_pointer_member_s_type_is_still_looked_up_where_the_call_is_a_pimpl",
        files: vec![
            SourceFile {
                path: "db/database.h",
                source: "#include <memory>\nclass SimpleDatabase {\n public:\n  class DescriptorIndex {\n   public:\n    bool AddFile() { return false; }\n  };\n};\nclass EncodedDatabase {\n public:\n  bool Add();\n private:\n  class DescriptorIndex;\n  std::unique_ptr<DescriptorIndex> index_;\n};\n",
            },
            SourceFile {
                path: "db/database.cc",
                source: "#include \"db/database.h\"\nclass EncodedDatabase::DescriptorIndex {\n public:\n  bool AddFile() { return true; }\n};\nbool EncodedDatabase::Add() {\n  return index_->AddFile();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "EncodedDatabase::Add",
                expected: json!([
                    {"name": "EncodedDatabase::Add", "edges": [
                        {"symbol": {"name": "EncodedDatabase::DescriptorIndex::AddFile"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-iterator-arrow.test.ts:186 — an element of a library type has no project method to call
#[tokio::test]
async fn cpp_auto_iterator_arrow_case_186_an_element_of_a_library_type_has_no_project_method_to_call()
 {
    check_case(Case {
        name: "cpp_auto_iterator_arrow_case_186_an_element_of_a_library_type_has_no_project_method_to_call",
        files: vec![
            SourceFile {
                path: "util/library.h",
                source: "class ObjectLibrary {\n public:\n  ObjectLibrary* get() { return this; }\n};\n",
            },
            SourceFile {
                path: "util/registry.cc",
                source: "#include <memory>\n#include <vector>\n#include \"util/library.h\"\nclass Registry {\n public:\n  bool Newest() const {\n    auto iter = libraries_.crbegin();\n    return iter->get() != nullptr;\n  }\n private:\n  std::vector<std::shared_ptr<ObjectLibrary>> libraries_;\n};\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Registry::Newest",
                expected: json!([
                    {"name": "Registry::Newest", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-iterator-arrow.test.ts:216 — a project class named like a library container keeps what its own `begin()` returns
#[tokio::test]
async fn cpp_auto_iterator_arrow_case_216_a_project_class_named_like_a_library_container_keeps_what_its_own_begin_ret()
 {
    check_case(Case {
        name: "cpp_auto_iterator_arrow_case_216_a_project_class_named_like_a_library_container_keeps_what_its_own_begin_ret",
        files: vec![
            SourceFile {
                path: "util/span.h",
                source: "class Foo {\n public:\n  void Bar() {}\n};\nclass Baz {\n public:\n  void Bar() {}\n};\nnamespace util {\nclass span {\n public:\n  Foo* begin() { return nullptr; }\n};\n}  // namespace util\n",
            },
            SourceFile {
                path: "util/use.cc",
                source: "#include \"util/span.h\"\nvoid Use(util::span s) {\n  auto it = s.begin();\n  it->Bar();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Use",
                expected: json!([
                    {"name": "Use", "edges": [
                        {"symbol": {"name": "Foo::Bar"}},
                        {"symbol": {"name": "util::span::begin"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-dereferenced-iterator-calls.test.ts:66 — reproduction: `(*it)->Visit()` over a `std::vector<Node*>`, and `(*it).Weight()` over a `std::vector<Node>`
#[tokio::test]
async fn cpp_dereferenced_iterator_calls_case_66_reproduction_it_visit_over_a_std_vector_node_and_it_weight_over_a_std_vecto()
 {
    check_case(Case {
        name: "cpp_dereferenced_iterator_calls_case_66_reproduction_it_visit_over_a_std_vector_node_and_it_weight_over_a_std_vecto",
        files: vec![
            SourceFile {
                path: "src/node.h",
                source: "class Other {\n public:\n  int Visit() { return 2; }\n  int Weight() const { return 3; }\n};\nclass Node {\n public:\n  int Visit() { return 0; }\n  int Weight() const { return 1; }\n  Node* Child() { return this; }\n};\n",
            },
            SourceFile {
                path: "src/walk.cc",
                source: "#include <vector>\n#include \"src/node.h\"\nint Walk(std::vector<Node*>& nodes) {\n  int sum = 0;\n  for (std::vector<Node*>::iterator it = nodes.begin(); it != nodes.end(); ++it) sum += (*it)->Visit();\n  return sum;\n}\nint Weigh(std::vector<Node>& values) {\n  int sum = 0;\n  for (auto vit = values.begin(); vit != values.end(); ++vit) sum += (*vit).Weight();\n  return sum;\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Walk",
                expected: json!([
                    {"name": "Walk", "edges": [
                        {"symbol": {"name": "Node::Visit"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Weigh",
                expected: json!([
                    {"name": "Weigh", "edges": [
                        {"symbol": {"name": "Node::Weight"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-dereferenced-iterator-calls.test.ts:92 — a local the dereference initializes, an element held in a smart pointer, and a raw pointer
#[tokio::test]
async fn cpp_dereferenced_iterator_calls_case_92_a_local_the_dereference_initializes_an_element_held_in_a_smart_pointer_and()
 {
    check_case(Case {
        name: "cpp_dereferenced_iterator_calls_case_92_a_local_the_dereference_initializes_an_element_held_in_a_smart_pointer_and",
        files: vec![
            SourceFile {
                path: "src/node.h",
                source: "class Other {\n public:\n  int Visit() { return 2; }\n  int Weight() const { return 3; }\n};\nclass Node {\n public:\n  int Visit() { return 0; }\n  int Weight() const { return 1; }\n  Node* Child() { return this; }\n};\n",
            },
            SourceFile {
                path: "src/first.cc",
                source: "#include <memory>\n#include <vector>\n#include \"src/node.h\"\nint First(std::vector<Node*>& nodes) {\n  auto first = *nodes.begin();\n  return first->Visit();\n}\nint Owned(std::vector<std::unique_ptr<Node>>& owned) {\n  auto oit = owned.begin();\n  return (*oit)->Weight();\n}\nint Pointed(Node* node) {\n  return (*node).Weight();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "First",
                expected: json!([
                    {"name": "First", "edges": [
                        {"symbol": {"name": "Node::Visit"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Owned",
                expected: json!([
                    {"name": "Owned", "edges": [
                        {"symbol": {"name": "Node::Weight"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Pointed",
                expected: json!([
                    {"name": "Pointed", "edges": [
                        {"symbol": {"name": "Node::Weight"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-dereferenced-iterator-calls.test.ts:121 — `->` through an iterator in a chain, and in what initializes a local
#[tokio::test]
async fn cpp_dereferenced_iterator_calls_case_121_through_an_iterator_in_a_chain_and_in_what_initializes_a_local()
 {
    check_case(Case {
        name: "cpp_dereferenced_iterator_calls_case_121_through_an_iterator_in_a_chain_and_in_what_initializes_a_local",
        files: vec![
            SourceFile {
                path: "src/node.h",
                source: "class Other {\n public:\n  int Visit() { return 2; }\n  int Weight() const { return 3; }\n};\nclass Node {\n public:\n  int Visit() { return 0; }\n  int Weight() const { return 1; }\n  Node* Child() { return this; }\n};\n",
            },
            SourceFile {
                path: "src/chain.cc",
                source: "#include <vector>\n#include \"src/node.h\"\nint Chain(std::vector<Node>& values) {\n  std::vector<Node>::iterator it = values.begin();\n  return it->Child()->Visit();\n}\nint Local(std::vector<Node>& values) {\n  std::vector<Node>::iterator it = values.begin();\n  auto child = it->Child();\n  return child->Visit();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Chain",
                expected: json!([
                    {"name": "Chain", "edges": [
                        {"symbol": {"name": "Node::Child"}},
                        {"symbol": {"name": "Node::Visit"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Local",
                expected: json!([
                    {"name": "Local", "edges": [
                        {"symbol": {"name": "Node::Child"}},
                        {"symbol": {"name": "Node::Visit"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-dereferenced-iterator-calls.test.ts:146 — an element of a library type has no project method to call, nor does a free function of the name
#[tokio::test]
async fn cpp_dereferenced_iterator_calls_case_146_an_element_of_a_library_type_has_no_project_method_to_call_nor_does_a_free()
 {
    check_case(Case {
        name: "cpp_dereferenced_iterator_calls_case_146_an_element_of_a_library_type_has_no_project_method_to_call_nor_does_a_free",
        files: vec![
            SourceFile {
                path: "src/node.h",
                source: "class Other {\n public:\n  int Visit() { return 2; }\n  int Weight() const { return 3; }\n};\nclass Node {\n public:\n  int Visit() { return 0; }\n  int Weight() const { return 1; }\n  Node* Child() { return this; }\n};\n",
            },
            SourceFile {
                path: "src/names.cc",
                source: "#include <string>\n#include <vector>\n#include \"src/node.h\"\nclass Label {\n public:\n  int size() const { return 0; }\n};\nint Visit() { return 9; }\nint Total(std::vector<std::string>& names) {\n  int n = 0;\n  for (auto sit = names.begin(); sit != names.end(); ++sit) n += (*sit).size();\n  return n;\n}\nint Each(std::vector<Node*>& nodes) {\n  std::vector<Node*>::iterator it = nodes.begin();\n  return (*it)->Visit();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Total",
                expected: json!([
                    {"name": "Total", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Each",
                expected: json!([
                    {"name": "Each", "edges": [
                        {"symbol": {"name": "Node::Visit"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-element-access-calls.test.ts:99 — reproduction: a vector's, a map's, and owning pointers' elements
#[tokio::test]
async fn cpp_element_access_calls_case_99_reproduction_a_vector_s_a_map_s_and_owning_pointers_elements()
 {
    check_case(Case {
        name: "cpp_element_access_calls_case_99_reproduction_a_vector_s_a_map_s_and_owning_pointers_elements",
        files: vec![
            SourceFile {
                path: "src/types.h",
                source: "class Other {\n public:\n  bool IsNotFound() const { return false; }\n  bool ok() const { return false; }\n  void Flush() {}\n  bool Valid() const { return false; }\n  int size() const { return 0; }\n  void Print() const {}\n  int Size() const { return 0; }\n};\nclass Status {\n public:\n  bool IsNotFound() const { return true; }\n  bool ok() const { return true; }\n};\nclass Buffer {\n public:\n  void Flush() {}\n};\nclass Iterator {\n public:\n  bool Valid() const { return true; }\n};\nclass Cell {\n public:\n  void Print() const {}\n};\nclass Row {\n public:\n  Cell& operator[](int i) { return cells_[i]; }\n  int Size() const { return 4; }\n private:\n  Cell cells_[4];\n};\n",
            },
            SourceFile {
                path: "src/access.cc",
                source: "#include <map>\n#include <memory>\n#include <vector>\n#include \"src/types.h\"\nbool Missing(std::vector<Status>& statuses, int i) { return statuses[i].IsNotFound(); }\nvoid FlushOne(std::map<int, Buffer>& buffers, int k) { buffers[k].Flush(); }\nbool AnyValid(std::vector<std::unique_ptr<Iterator>>& iters, int i) { return iters[i]->Valid(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Missing",
                expected: json!([
                    {"name": "Missing", "edges": [
                        {"symbol": {"name": "Status::IsNotFound"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "FlushOne",
                expected: json!([
                    {"name": "FlushOne", "edges": [
                        {"symbol": {"name": "Buffer::Flush"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "AnyValid",
                expected: json!([
                    {"name": "AnyValid", "edges": [
                        {"symbol": {"name": "Iterator::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-element-access-calls.test.ts:121 — a raw array, a pointer, two subscripts, a pair's member, and a local the element initializes
#[tokio::test]
async fn cpp_element_access_calls_case_121_a_raw_array_a_pointer_two_subscripts_a_pair_s_member_and_a_local_the_elemen()
 {
    check_case(Case {
        name: "cpp_element_access_calls_case_121_a_raw_array_a_pointer_two_subscripts_a_pair_s_member_and_a_local_the_elemen",
        files: vec![
            SourceFile {
                path: "src/types.h",
                source: "class Other {\n public:\n  bool IsNotFound() const { return false; }\n  bool ok() const { return false; }\n  void Flush() {}\n  bool Valid() const { return false; }\n  int size() const { return 0; }\n  void Print() const {}\n  int Size() const { return 0; }\n};\nclass Status {\n public:\n  bool IsNotFound() const { return true; }\n  bool ok() const { return true; }\n};\nclass Buffer {\n public:\n  void Flush() {}\n};\nclass Iterator {\n public:\n  bool Valid() const { return true; }\n};\nclass Cell {\n public:\n  void Print() const {}\n};\nclass Row {\n public:\n  Cell& operator[](int i) { return cells_[i]; }\n  int Size() const { return 4; }\n private:\n  Cell cells_[4];\n};\n",
            },
            SourceFile {
                path: "src/more.cc",
                source: "#include <utility>\n#include <vector>\n#include \"src/types.h\"\nbool Array() { Status statuses[3]; return statuses[0].ok(); }\nbool Pointer(Status* statuses, int i) { return statuses[i].ok(); }\nvoid Grid(std::vector<std::vector<Buffer>>& table) { table[0][1].Flush(); }\nvoid Pairs(std::vector<std::pair<Buffer, int>>& cfs, int i) { cfs[i].first.Flush(); }\nbool Bound(std::vector<Status>& statuses, int i) {\n  auto& s = statuses[i];\n  return s.ok();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Array",
                expected: json!([
                    {"name": "Array", "edges": [
                        {"symbol": {"name": "Status::ok"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Pointer",
                expected: json!([
                    {"name": "Pointer", "edges": [
                        {"symbol": {"name": "Status::ok"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Grid",
                expected: json!([
                    {"name": "Grid", "edges": [
                        {"symbol": {"name": "Buffer::Flush"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Pairs",
                expected: json!([
                    {"name": "Pairs", "edges": [
                        {"symbol": {"name": "Buffer::Flush"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Bound",
                expected: json!([
                    {"name": "Bound", "edges": [
                        {"symbol": {"name": "Status::ok"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-element-access-calls.test.ts:149 — a class's `operator[]`, a key with a `]` in it, and an element of a library type
#[tokio::test]
async fn cpp_element_access_calls_case_149_a_class_s_operator_a_key_with_a_in_it_and_an_element_of_a_library_type()
 {
    check_case(Case {
        name: "cpp_element_access_calls_case_149_a_class_s_operator_a_key_with_a_in_it_and_an_element_of_a_library_type",
        files: vec![
            SourceFile {
                path: "src/types.h",
                source: "class Other {\n public:\n  bool IsNotFound() const { return false; }\n  bool ok() const { return false; }\n  void Flush() {}\n  bool Valid() const { return false; }\n  int size() const { return 0; }\n  void Print() const {}\n  int Size() const { return 0; }\n};\nclass Status {\n public:\n  bool IsNotFound() const { return true; }\n  bool ok() const { return true; }\n};\nclass Buffer {\n public:\n  void Flush() {}\n};\nclass Iterator {\n public:\n  bool Valid() const { return true; }\n};\nclass Cell {\n public:\n  void Print() const {}\n};\nclass Row {\n public:\n  Cell& operator[](int i) { return cells_[i]; }\n  int Size() const { return 4; }\n private:\n  Cell cells_[4];\n};\n",
            },
            SourceFile {
                path: "src/rows.cc",
                source: "#include <map>\n#include <string>\n#include <vector>\n#include \"src/types.h\"\nvoid Show(Row& row) { row[0].Print(); }\nvoid Named(std::map<std::string, Buffer>& buffers) { buffers[\"a]b\"].Flush(); }\nint Length(std::vector<std::string>& values, int i) { return values[i].size(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Show",
                expected: json!([
                    {"name": "Show", "edges": [
                        {"symbol": {"name": "Cell::Print"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Named",
                expected: json!([
                    {"name": "Named", "edges": [
                        {"symbol": {"name": "Buffer::Flush"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Length",
                expected: json!([
                    {"name": "Length", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-element-access-calls.test.ts:171 — an array is of the type it is declared with, however many dimensions it has
#[tokio::test]
async fn cpp_element_access_calls_case_171_an_array_is_of_the_type_it_is_declared_with_however_many_dimensions_it_has()
 {
    check_case(Case {
        name: "cpp_element_access_calls_case_171_an_array_is_of_the_type_it_is_declared_with_however_many_dimensions_it_has",
        files: vec![
            SourceFile {
                path: "src/types.h",
                source: "class Other {\n public:\n  bool IsNotFound() const { return false; }\n  bool ok() const { return false; }\n  void Flush() {}\n  bool Valid() const { return false; }\n  int size() const { return 0; }\n  void Print() const {}\n  int Size() const { return 0; }\n};\nclass Status {\n public:\n  bool IsNotFound() const { return true; }\n  bool ok() const { return true; }\n};\nclass Buffer {\n public:\n  void Flush() {}\n};\nclass Iterator {\n public:\n  bool Valid() const { return true; }\n};\nclass Cell {\n public:\n  void Print() const {}\n};\nclass Row {\n public:\n  Cell& operator[](int i) { return cells_[i]; }\n  int Size() const { return 4; }\n private:\n  Cell cells_[4];\n};\n",
            },
            SourceFile {
                path: "src/arrays.cc",
                source: "#include <vector>\n#include \"src/types.h\"\nclass Shards {\n public:\n  bool Second() { return statuses_[1].ok(); }\n private:\n  Status statuses_[2];\n};\nint Rows() { Row rows[2]; return rows[1].Size(); }\nusing Line = Row;\nint Lines() { Line lines[2]; return lines[1].Size(); }\nbool Grid() { Status grid[2][3]; return grid[1][2].ok(); }\nvoid Pools() { std::vector<Buffer> pools[2]; pools[1][0].Flush(); }\nbool First() { Status statuses[2]; return (*statuses).ok(); }\nbool Each() {\n  Status statuses[2];\n  for (auto& s : statuses) {\n    if (!s.ok()) return false;\n  }\n  return true;\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Shards::Second",
                expected: json!([
                    {"name": "Shards::Second", "edges": [
                        {"symbol": {"name": "Status::ok"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Rows",
                expected: json!([
                    {"name": "Rows", "edges": [
                        {"symbol": {"name": "Row::Size"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Lines",
                expected: json!([
                    {"name": "Lines", "edges": [
                        {"symbol": {"name": "Row::Size"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Grid",
                expected: json!([
                    {"name": "Grid", "edges": [
                        {"symbol": {"name": "Status::ok"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Pools",
                expected: json!([
                    {"name": "Pools", "edges": [
                        {"symbol": {"name": "Buffer::Flush"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "First",
                expected: json!([
                    {"name": "First", "edges": [
                        {"symbol": {"name": "Status::ok"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Each",
                expected: json!([
                    {"name": "Each", "edges": [
                        {"symbol": {"name": "Status::ok"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-element-access-calls.test.ts:211 — an element read by subscript picks the overload it is passed to
#[tokio::test]
async fn cpp_element_access_calls_case_211_an_element_read_by_subscript_picks_the_overload_it_is_passed_to()
 {
    check_case(Case {
        name: "cpp_element_access_calls_case_211_an_element_read_by_subscript_picks_the_overload_it_is_passed_to",
        files: vec![
            SourceFile {
                path: "src/types.h",
                source: "class Other {\n public:\n  bool IsNotFound() const { return false; }\n  bool ok() const { return false; }\n  void Flush() {}\n  bool Valid() const { return false; }\n  int size() const { return 0; }\n  void Print() const {}\n  int Size() const { return 0; }\n};\nclass Status {\n public:\n  bool IsNotFound() const { return true; }\n  bool ok() const { return true; }\n};\nclass Buffer {\n public:\n  void Flush() {}\n};\nclass Iterator {\n public:\n  bool Valid() const { return true; }\n};\nclass Cell {\n public:\n  void Print() const {}\n};\nclass Row {\n public:\n  Cell& operator[](int i) { return cells_[i]; }\n  int Size() const { return 4; }\n private:\n  Cell cells_[4];\n};\n",
            },
            SourceFile {
                path: "src/sink.cc",
                source: "#include <map>\n#include \"src/types.h\"\nclass Sink {\n public:\n  void Put(const Other& o) {}\n  void Put(const Buffer& b) {}\n};\nvoid Drain(Sink& sink, std::map<int, Buffer>& buffers) { sink.Put(buffers[1]); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Drain",
                expected: json!([
                    {"name": "Drain", "edges": [
                        {"symbol": {"name": "Sink::Put", "startLine": 6}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-declared-type-receiver.test.ts:109 — reproduction: `Iterator* iter
// iter->Valid()` is the Iterator the scope sees, not a nested namesake
#[tokio::test]
async fn cpp_declared_type_receiver_case_109_reproduction_iterator_iter_iter_valid_is_the_iterator_the_scope_sees_not_a()
 {
    check_case(Case {
        name: "cpp_declared_type_receiver_case_109_reproduction_iterator_iter_iter_valid_is_the_iterator_the_scope_sees_not_a",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator_base.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual ~IteratorBase() {}\n  virtual bool Valid() const = 0;\n  virtual void Next() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "#include \"include/rocksdb/iterator_base.h\"\nnamespace rocksdb {\nclass Iterator : public IteratorBase {\n public:\n  virtual int value() const = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/memtablerep.h",
                source: "namespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    virtual bool Valid() const = 0;\n    virtual void Next() = 0;\n  };\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/db.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DB {\n public:\n  virtual Iterator* NewIterator() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_impl.cc",
                source: "#include \"include/rocksdb/db.h\"\n#include \"include/rocksdb/memtablerep.h\"\nnamespace rocksdb {\nint CountKeys(DB* db) {\n  Iterator* iter = db->NewIterator();\n  int n = 0;\n  for (; iter->Valid(); iter->Next()) n += iter->value();\n  delete iter;\n  return n;\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::CountKeys",
                expected: json!([
                    {"name": "rocksdb::CountKeys", "edges": [
                        {"symbol": {"name": "rocksdb::DB::NewIterator"}},
                        {"symbol": {"name": "rocksdb::Iterator::value"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::Next"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-declared-type-receiver.test.ts:139 — a reference parameter, a value, and a member the class declares in its header
#[tokio::test]
async fn cpp_declared_type_receiver_case_139_a_reference_parameter_a_value_and_a_member_the_class_declares_in_its_header()
 {
    check_case(Case {
        name: "cpp_declared_type_receiver_case_139_a_reference_parameter_a_value_and_a_member_the_class_declares_in_its_header",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator_base.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual ~IteratorBase() {}\n  virtual bool Valid() const = 0;\n  virtual void Next() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "#include \"include/rocksdb/iterator_base.h\"\nnamespace rocksdb {\nclass Iterator : public IteratorBase {\n public:\n  virtual int value() const = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/memtablerep.h",
                source: "namespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    virtual bool Valid() const = 0;\n    virtual void Next() = 0;\n  };\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/db.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DB {\n public:\n  virtual Iterator* NewIterator() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "util/bit_fields.h",
                source: "namespace rocksdb {\nclass BitFields {\n public:\n  int Get() const { return 0; }\n};\nclass ConcurrentTest {\n public:\n  struct State {\n    int Get() const { return 1; }\n  };\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/compaction_job.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass CompactionJob {\n public:\n  bool Advance();\n private:\n  Iterator* input_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/compaction_job.cc",
                source: "#include \"db/compaction_job.h\"\n#include \"include/rocksdb/memtablerep.h\"\n#include \"util/bit_fields.h\"\nnamespace rocksdb {\nstruct State : public BitFields {};\nbool IsValid(const Iterator& it) {\n  return it.Valid();\n}\nint Slot() {\n  State state;\n  return state.Get();\n}\nbool CompactionJob::Advance() {\n  input_->Next();\n  return input_->Valid();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::IsValid",
                expected: json!([
                    {"name": "rocksdb::IsValid", "edges": [
                        {"symbol": {"name": "rocksdb::IteratorBase::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::Slot",
                expected: json!([
                    {"name": "rocksdb::Slot", "edges": [
                        {"symbol": {"name": "rocksdb::BitFields::Get"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::CompactionJob::Advance",
                expected: json!([
                    {"name": "rocksdb::CompactionJob::Advance", "edges": [
                        {"symbol": {"name": "rocksdb::IteratorBase::Next"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-declared-type-receiver.test.ts:197 — a nested class is found first from inside a class that inherits it
#[tokio::test]
async fn cpp_declared_type_receiver_case_197_a_nested_class_is_found_first_from_inside_a_class_that_inherits_it()
 {
    check_case(Case {
        name: "cpp_declared_type_receiver_case_197_a_nested_class_is_found_first_from_inside_a_class_that_inherits_it",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator_base.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual ~IteratorBase() {}\n  virtual bool Valid() const = 0;\n  virtual void Next() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "#include \"include/rocksdb/iterator_base.h\"\nnamespace rocksdb {\nclass Iterator : public IteratorBase {\n public:\n  virtual int value() const = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/memtablerep.h",
                source: "namespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    virtual bool Valid() const = 0;\n    virtual void Next() = 0;\n  };\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/db.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DB {\n public:\n  virtual Iterator* NewIterator() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "memtable/skiplistrep.cc",
                source: "#include \"include/rocksdb/iterator.h\"\n#include \"include/rocksdb/memtablerep.h\"\nnamespace rocksdb {\nclass SkipListRep : public MemTableRep {\n public:\n  bool Check();\n private:\n  Iterator* iter_;\n};\nbool SkipListRep::Check() { return iter_->Valid(); }\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::SkipListRep::Check",
                expected: json!([
                    {"name": "rocksdb::SkipListRep::Check", "edges": [
                        {"symbol": {"name": "rocksdb::MemTableRep::Iterator::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-declared-type-receiver.test.ts:222 — a `using` in the namespace around the call comes before a global class of that name
#[tokio::test]
async fn cpp_declared_type_receiver_case_222_a_using_in_the_namespace_around_the_call_comes_before_a_global_class_of_tha()
 {
    check_case(Case {
        name: "cpp_declared_type_receiver_case_222_a_using_in_the_namespace_around_the_call_comes_before_a_global_class_of_tha",
        files: vec![
            SourceFile {
                path: "php/ext/google/protobuf/def.h",
                source: "typedef struct Descriptor {\n  int index;\n} Descriptor;\n",
            },
            SourceFile {
                path: "src/google/protobuf/descriptor.h",
                source: "namespace google {\nnamespace protobuf {\nclass Descriptor {\n public:\n  int FindFieldByNumber(int number) const { return number; }\n};\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "upb_generator/common/cpp_to_upb_def.cc",
                source: "#include \"src/google/protobuf/descriptor.h\"\nnamespace upb {\nnamespace generator {\nusing google::protobuf::Descriptor;\nint FindField(const Descriptor* message, int number) {\n  return message->FindFieldByNumber(number);\n}\n}  // namespace generator\n}  // namespace upb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "upb::generator::FindField",
                expected: json!([
                    {"name": "upb::generator::FindField", "edges": [
                        {"symbol": {"name": "google::protobuf::Descriptor::FindFieldByNumber"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-declared-type-receiver.test.ts:263 — a method the class only inherits from a library type
#[tokio::test]
async fn cpp_declared_type_receiver_case_263_a_method_the_class_only_inherits_from_a_library_type()
{
    check_case(Case {
        name: "cpp_declared_type_receiver_case_263_a_method_the_class_only_inherits_from_a_library_type",
        files: vec![
            SourceFile {
                path: "java/rocksjni/kv_helper.h",
                source: "namespace rocksdb {\nclass KVException {\n public:\n  const char* what() const { return \"\"; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/multi_scan.h",
                source: "#include <stdexcept>\nnamespace rocksdb {\nclass MultiScanException : public std::runtime_error {\n public:\n  explicit MultiScanException(const char* message) : std::runtime_error(message) {}\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "tools/db_bench_tool.cc",
                source: "#include \"include/rocksdb/multi_scan.h\"\nnamespace rocksdb {\nconst char* Describe(const MultiScanException& ex) {\n  return ex.what();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Describe",
                expected: json!([
                    {"name": "rocksdb::Describe", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-declared-type-receiver.test.ts:301 — a template parameter, and a class another source file defines
#[tokio::test]
async fn cpp_declared_type_receiver_case_301_a_template_parameter_and_a_class_another_source_file_defines()
 {
    check_case(Case {
        name: "cpp_declared_type_receiver_case_301_a_template_parameter_and_a_class_another_source_file_defines",
        files: vec![
            SourceFile {
                path: "util/heap.h",
                source: "namespace rocksdb {\nclass BinaryHeap {\n public:\n  void push(int v) {}\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/multi_cf_iterator_impl.h",
                source: "#include \"util/heap.h\"\nnamespace rocksdb {\ntemplate <typename BinaryHeap>\nclass MultiCfIteratorImpl {\n public:\n  void Reset(BinaryHeap& heap) { heap.push(1); }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/version_set.cc",
                source: "namespace rocksdb {\nclass FilePicker {\n public:\n  int GetNextFile() { return 0; }\n};\n}  // namespace rocksdb\n#include \"db/version_set_sync_and_async.h\"\n",
            },
            SourceFile {
                path: "db/version_set_sync_and_async.h",
                source: "namespace rocksdb {\nint NextFile() {\n  FilePicker fp;\n  return fp.GetNextFile();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::MultiCfIteratorImpl::Reset",
                expected: json!([
                    {"name": "rocksdb::MultiCfIteratorImpl::Reset", "edges": [
                        {"symbol": {"name": "rocksdb::BinaryHeap::push"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::NextFile",
                expected: json!([
                    {"name": "rocksdb::NextFile", "edges": [
                        {"symbol": {"name": "rocksdb::FilePicker::GetNextFile"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-declared-type-receiver.test.ts:350 — a class with a base the index does not link, which may declare the method
#[tokio::test]
async fn cpp_declared_type_receiver_case_350_a_class_with_a_base_the_index_does_not_link_which_may_declare_the_method()
 {
    check_case(Case {
        name: "cpp_declared_type_receiver_case_350_a_class_with_a_base_the_index_does_not_link_which_may_declare_the_method",
        files: vec![
            SourceFile {
                path: "src/google/protobuf/descriptor.cc",
                source: "namespace google {\nnamespace protobuf {\nnamespace internal {\ntemplate <typename... T>\nclass FlatAllocatorImpl {\n public:\n  void PlanArray(int array_size) {}\n};\ntemplate <typename... T>\nFlatAllocatorImpl<T...> ApplyTypeList(int);\nclass FlatAllocator : public decltype(ApplyTypeList<char, int>(0)) {};\n}  // namespace internal\nvoid PlanAllocationSize(internal::FlatAllocator& alloc) {\n  alloc.PlanArray(1);\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::PlanAllocationSize",
                expected: json!([
                    {"name": "google::protobuf::PlanAllocationSize", "edges": [
                        {"symbol": {"name": "google::protobuf::internal::FlatAllocatorImpl::PlanArray"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-declared-type-receiver.test.ts:380 — a declaration read from outside the calling function and its class
#[tokio::test]
async fn cpp_declared_type_receiver_case_380_a_declaration_read_from_outside_the_calling_function_and_its_class()
 {
    check_case(Case {
        name: "cpp_declared_type_receiver_case_380_a_declaration_read_from_outside_the_calling_function_and_its_class",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator_base.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual ~IteratorBase() {}\n  virtual bool Valid() const = 0;\n  virtual void Next() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "#include \"include/rocksdb/iterator_base.h\"\nnamespace rocksdb {\nclass Iterator : public IteratorBase {\n public:\n  virtual int value() const = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/memtablerep.h",
                source: "namespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    virtual bool Valid() const = 0;\n    virtual void Next() = 0;\n  };\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/db.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DB {\n public:\n  virtual Iterator* NewIterator() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/table_cursor.h",
                source: "#include \"include/rocksdb/memtablerep.h\"\nnamespace rocksdb {\nextern MemTableRep::Iterator* iter;\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_iter.cc",
                source: "#include \"include/rocksdb/iterator.h\"\n#include \"db/table_cursor.h\"\nnamespace rocksdb {\nvoid Prepare() {\n  Iterator* iter = nullptr;\n}\nbool Step() {\n  return iter->Valid();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Step",
                expected: json!([
                    {"name": "rocksdb::Step", "edges": [
                        {"symbol": {"name": "rocksdb::MemTableRep::Iterator::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-library-type-receiver.test.ts:101 — reproduction: `std::string saved_key_
// saved_key_.clear()` is not Slice::clear
#[tokio::test]
async fn cpp_library_type_receiver_case_101_reproduction_std_string_saved_key_saved_key_clear_is_not_slice_clear()
 {
    check_case(Case {
        name: "cpp_library_type_receiver_case_101_reproduction_std_string_saved_key_saved_key_clear_is_not_slice_clear",
        files: vec![
            SourceFile {
                path: "include/leveldb/slice.h",
                source: "namespace leveldb {\nclass Slice {\n public:\n  const char* data() const { return data_; }\n  unsigned size() const { return size_; }\n  void clear() { size_ = 0; }\n  const char* begin() const { return data_; }\n  void remove_prefix(unsigned n) { size_ -= n; }\n private:\n  const char* data_;\n  unsigned size_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/buffers.h",
                source: "namespace leveldb {\nclass LeftoverBuffer {\n public:\n  void append(const char* s, unsigned n) {}\n};\nclass Arena {\n public:\n  void insert(int block) {}\n};\nclass Cache {\n public:\n  void reset() {}\n};\nclass Iterator {\n public:\n  void SeekToFirst() {}\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/db_iter.cc",
                source: "#include <string>\n#include \"include/leveldb/slice.h\"\nnamespace leveldb {\nclass DBIter {\n public:\n  void Next();\n private:\n  std::string saved_key_;    // == current key when direction_==kReverse\n};\nvoid DBIter::Next() {\n  saved_key_.clear();\n}\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "leveldb::Slice::clear",
                expected: json!([
                    {"name": "leveldb::Slice::clear", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-library-type-receiver.test.ts:128 — a local, a parameter, a raw pointer, nested template arguments and an abseil container
#[tokio::test]
async fn cpp_library_type_receiver_case_128_a_local_a_parameter_a_raw_pointer_nested_template_arguments_and_an_abseil_c()
 {
    check_case(Case {
        name: "cpp_library_type_receiver_case_128_a_local_a_parameter_a_raw_pointer_nested_template_arguments_and_an_abseil_c",
        files: vec![
            SourceFile {
                path: "include/leveldb/slice.h",
                source: "namespace leveldb {\nclass Slice {\n public:\n  const char* data() const { return data_; }\n  unsigned size() const { return size_; }\n  void clear() { size_ = 0; }\n  const char* begin() const { return data_; }\n  void remove_prefix(unsigned n) { size_ -= n; }\n private:\n  const char* data_;\n  unsigned size_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/buffers.h",
                source: "namespace leveldb {\nclass LeftoverBuffer {\n public:\n  void append(const char* s, unsigned n) {}\n};\nclass Arena {\n public:\n  void insert(int block) {}\n};\nclass Cache {\n public:\n  void reset() {}\n};\nclass Iterator {\n public:\n  void SeekToFirst() {}\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/encode.cc",
                source: "#include <string>\n#include <vector>\n#include \"absl/container/flat_hash_set.h\"\n#include \"include/leveldb/slice.h\"\n#include \"util/buffers.h\"\nnamespace leveldb {\nvoid Encode(std::string* dst, const Slice& value) {\n  dst->clear();\n  dst->append(value.data(), value.size());\n}\nstd::string Render(const std::vector<std::pair<int, Slice>>& files) {\n  std::string proto;\n  proto.append(\"x\", 1);\n  absl::flat_hash_set<int> seen;\n  seen.insert(1);\n  const char* first = proto.data();\n  return files.begin() == files.end() ? proto : std::string(first);\n}\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::Encode",
                expected: json!([
                    {"name": "leveldb::Encode", "edges": [
                        {"symbol": {"name": "leveldb::Slice::data"}},
                        {"symbol": {"name": "leveldb::Slice::size"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "leveldb::Render",
                expected: json!([
                    {"name": "leveldb::Render", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-library-type-receiver.test.ts:162 — a `std::` name beside a project alias of the same name elsewhere
#[tokio::test]
async fn cpp_library_type_receiver_case_162_a_std_name_beside_a_project_alias_of_the_same_name_elsewhere()
 {
    check_case(Case {
        name: "cpp_library_type_receiver_case_162_a_std_name_beside_a_project_alias_of_the_same_name_elsewhere",
        files: vec![
            SourceFile {
                path: "include/leveldb/slice.h",
                source: "namespace leveldb {\nclass Slice {\n public:\n  const char* data() const { return data_; }\n  unsigned size() const { return size_; }\n  void clear() { size_ = 0; }\n  const char* begin() const { return data_; }\n  void remove_prefix(unsigned n) { size_ -= n; }\n private:\n  const char* data_;\n  unsigned size_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/buffers.h",
                source: "namespace leveldb {\nclass LeftoverBuffer {\n public:\n  void append(const char* s, unsigned n) {}\n};\nclass Arena {\n public:\n  void insert(int block) {}\n};\nclass Cache {\n public:\n  void reset() {}\n};\nclass Iterator {\n public:\n  void SeekToFirst() {}\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "third_party/gtest.h",
                source: "#include <string>\nnamespace testing {\nnamespace internal {\ntypedef ::std::string string;\n}  // namespace internal\n}  // namespace testing\n",
            },
            SourceFile {
                path: "tools/trim.cc",
                source: "#include <string>\n#include \"include/leveldb/slice.h\"\nvoid Trim(std::string& text) {\n  text.clear();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Trim",
                expected: json!([
                    {"name": "Trim", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-library-type-receiver.test.ts:190 — a smart pointer: `.` is its own member, `->` reaches the element type
#[tokio::test]
async fn cpp_library_type_receiver_case_190_a_smart_pointer_is_its_own_member_reaches_the_element_type()
 {
    check_case(Case {
        name: "cpp_library_type_receiver_case_190_a_smart_pointer_is_its_own_member_reaches_the_element_type",
        files: vec![
            SourceFile {
                path: "include/leveldb/slice.h",
                source: "namespace leveldb {\nclass Slice {\n public:\n  const char* data() const { return data_; }\n  unsigned size() const { return size_; }\n  void clear() { size_ = 0; }\n  const char* begin() const { return data_; }\n  void remove_prefix(unsigned n) { size_ -= n; }\n private:\n  const char* data_;\n  unsigned size_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/buffers.h",
                source: "namespace leveldb {\nclass LeftoverBuffer {\n public:\n  void append(const char* s, unsigned n) {}\n};\nclass Arena {\n public:\n  void insert(int block) {}\n};\nclass Cache {\n public:\n  void reset() {}\n};\nclass Iterator {\n public:\n  void SeekToFirst() {}\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/scan.cc",
                source: "#include <memory>\n#include \"util/buffers.h\"\nnamespace leveldb {\nvoid Scan(Iterator* raw) {\n  std::unique_ptr<Iterator> iter(raw);\n  iter->SeekToFirst();\n  iter.reset();\n}\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::Scan",
                expected: json!([
                    {"name": "leveldb::Scan", "edges": [
                        {"symbol": {"name": "leveldb::Iterator::SeekToFirst"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-library-type-receiver.test.ts:213 — a std trait or member type names another type, maybe the project's
#[tokio::test]
async fn cpp_library_type_receiver_case_213_a_std_trait_or_member_type_names_another_type_maybe_the_project_s()
 {
    check_case(Case {
        name: "cpp_library_type_receiver_case_213_a_std_trait_or_member_type_names_another_type_maybe_the_project_s",
        files: vec![
            SourceFile {
                path: "include/leveldb/slice.h",
                source: "namespace leveldb {\nclass Slice {\n public:\n  const char* data() const { return data_; }\n  unsigned size() const { return size_; }\n  void clear() { size_ = 0; }\n  const char* begin() const { return data_; }\n  void remove_prefix(unsigned n) { size_ -= n; }\n private:\n  const char* data_;\n  unsigned size_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/buffers.h",
                source: "namespace leveldb {\nclass LeftoverBuffer {\n public:\n  void append(const char* s, unsigned n) {}\n};\nclass Arena {\n public:\n  void insert(int block) {}\n};\nclass Cache {\n public:\n  void reset() {}\n};\nclass Iterator {\n public:\n  void SeekToFirst() {}\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/compression.cc",
                source: "#include <type_traits>\n#include <vector>\n#include \"include/leveldb/slice.h\"\nnamespace leveldb {\ntemplate <bool kIsDigestedDict>\nunsigned DictSize(std::conditional_t<kIsDigestedDict, void*, Slice> dict) {\n  return dict.size();\n}\nvoid Drop(const std::vector<Slice>& slices) {\n  std::vector<Slice>::value_type first = slices[0];\n  first.remove_prefix(1);\n}\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::DictSize",
                expected: json!([
                    {"name": "leveldb::DictSize", "edges": [
                        {"symbol": {"name": "leveldb::Slice::size"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "leveldb::Drop",
                expected: json!([
                    {"name": "leveldb::Drop", "edges": [
                        {"symbol": {"name": "leveldb::Slice::remove_prefix"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-library-type-receiver.test.ts:242 — a project type with a lowercase name is still the project's
#[tokio::test]
async fn cpp_library_type_receiver_case_242_a_project_type_with_a_lowercase_name_is_still_the_project_s()
 {
    check_case(Case {
        name: "cpp_library_type_receiver_case_242_a_project_type_with_a_lowercase_name_is_still_the_project_s",
        files: vec![
            SourceFile {
                path: "include/leveldb/slice.h",
                source: "namespace leveldb {\nclass Slice {\n public:\n  const char* data() const { return data_; }\n  unsigned size() const { return size_; }\n  void clear() { size_ = 0; }\n  const char* begin() const { return data_; }\n  void remove_prefix(unsigned n) { size_ -= n; }\n private:\n  const char* data_;\n  unsigned size_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/buffers.h",
                source: "namespace leveldb {\nclass LeftoverBuffer {\n public:\n  void append(const char* s, unsigned n) {}\n};\nclass Arena {\n public:\n  void insert(int block) {}\n};\nclass Cache {\n public:\n  void reset() {}\n};\nclass Iterator {\n public:\n  void SeekToFirst() {}\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/arena.h",
                source: "namespace util {\nclass arena {\n public:\n  void* allocate(unsigned n) { return 0; }\n};\n}  // namespace util\n",
            },
            SourceFile {
                path: "util/use.cc",
                source: "#include \"util/arena.h\"\nvoid Use(util::arena& a) {\n  a.allocate(8);\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Use",
                expected: json!([
                    {"name": "Use", "edges": [
                        {"symbol": {"name": "util::arena::allocate"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-library-type-receiver.test.ts:269 — a range-for variable, or an `auto` one, hides an earlier library-typed one of the same name
#[tokio::test]
async fn cpp_library_type_receiver_case_269_a_range_for_variable_or_an_auto_one_hides_an_earlier_library_typed_one_of_t()
 {
    check_case(Case {
        name: "cpp_library_type_receiver_case_269_a_range_for_variable_or_an_auto_one_hides_an_earlier_library_typed_one_of_t",
        files: vec![
            SourceFile {
                path: "include/leveldb/slice.h",
                source: "namespace leveldb {\nclass Slice {\n public:\n  const char* data() const { return data_; }\n  unsigned size() const { return size_; }\n  void clear() { size_ = 0; }\n  const char* begin() const { return data_; }\n  void remove_prefix(unsigned n) { size_ -= n; }\n private:\n  const char* data_;\n  unsigned size_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/buffers.h",
                source: "namespace leveldb {\nclass LeftoverBuffer {\n public:\n  void append(const char* s, unsigned n) {}\n};\nclass Arena {\n public:\n  void insert(int block) {}\n};\nclass Cache {\n public:\n  void reset() {}\n};\nclass Iterator {\n public:\n  void SeekToFirst() {}\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/shadow.cc",
                source: "#include <string>\n#include <vector>\n#include \"include/leveldb/slice.h\"\n#include \"util/buffers.h\"\nnamespace leveldb {\nvoid Trim(const std::vector<Slice>& slices) {\n  std::string key;\n  for (const Slice& key : slices) {\n    key.remove_prefix(1);\n  }\n}\nvoid Flush(Cache* caches) {\n  { std::string buf; buf.clear(); }\n  auto buf = caches[0];\n  buf.reset();\n}\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::Trim",
                expected: json!([
                    {"name": "leveldb::Trim", "edges": [
                        {"symbol": {"name": "leveldb::Slice::remove_prefix"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "leveldb::Flush",
                expected: json!([
                    {"name": "leveldb::Flush", "edges": [
                        {"symbol": {"name": "leveldb::Cache::reset"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-library-type-receiver.test.ts:301 — a call that contradicts the declaration it found: `.` on what reads as a pointer
#[tokio::test]
async fn cpp_library_type_receiver_case_301_a_call_that_contradicts_the_declaration_it_found_on_what_reads_as_a_pointer()
 {
    check_case(Case {
        name: "cpp_library_type_receiver_case_301_a_call_that_contradicts_the_declaration_it_found_on_what_reads_as_a_pointer",
        files: vec![
            SourceFile {
                path: "include/leveldb/slice.h",
                source: "namespace leveldb {\nclass Slice {\n public:\n  const char* data() const { return data_; }\n  unsigned size() const { return size_; }\n  void clear() { size_ = 0; }\n  const char* begin() const { return data_; }\n  void remove_prefix(unsigned n) { size_ -= n; }\n private:\n  const char* data_;\n  unsigned size_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/buffers.h",
                source: "namespace leveldb {\nclass LeftoverBuffer {\n public:\n  void append(const char* s, unsigned n) {}\n};\nclass Arena {\n public:\n  void insert(int block) {}\n};\nclass Cache {\n public:\n  void reset() {}\n};\nclass Iterator {\n public:\n  void SeekToFirst() {}\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/weight.cc",
                source: "#include \"include/leveldb/slice.h\"\nnamespace leveldb {\nunsigned Weight(const Slice& key) {\n  unsigned w = absl::kWeightScale * key[0];\n  return w + key.size();\n}\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::Weight",
                expected: json!([
                    {"name": "leveldb::Weight", "edges": [
                        {"symbol": {"name": "leveldb::Slice::size"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-library-type-receiver.test.ts:324 — a constant of a project class read as a type, and a scalar
#[tokio::test]
async fn cpp_library_type_receiver_case_324_a_constant_of_a_project_class_read_as_a_type_and_a_scalar()
 {
    check_case(Case {
        name: "cpp_library_type_receiver_case_324_a_constant_of_a_project_class_read_as_a_type_and_a_scalar",
        files: vec![
            SourceFile {
                path: "include/leveldb/slice.h",
                source: "namespace leveldb {\nclass Slice {\n public:\n  const char* data() const { return data_; }\n  unsigned size() const { return size_; }\n  void clear() { size_ = 0; }\n  const char* begin() const { return data_; }\n  void remove_prefix(unsigned n) { size_ -= n; }\n private:\n  const char* data_;\n  unsigned size_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/buffers.h",
                source: "namespace leveldb {\nclass LeftoverBuffer {\n public:\n  void append(const char* s, unsigned n) {}\n};\nclass Arena {\n public:\n  void insert(int block) {}\n};\nclass Cache {\n public:\n  void reset() {}\n};\nclass Iterator {\n public:\n  void SeekToFirst() {}\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/mask.cc",
                source: "#include <cstddef>\n#include \"include/leveldb/slice.h\"\n#define SLICE_AT(var, list, i) const Slice& var = list[i]\nnamespace leveldb {\nunsigned Mask(const Slice& key) {\n  unsigned m = Slice::kMask & key[0];\n  return m + key.size();\n}\nvoid Visit(const Slice* slices) {\n  std::size_t key = 0;\n  {\n    SLICE_AT(key, slices, 0);\n    key.remove_prefix(1);\n  }\n}\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::Mask",
                expected: json!([
                    {"name": "leveldb::Mask", "edges": [
                        {"symbol": {"name": "leveldb::Slice::size"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "leveldb::Visit",
                expected: json!([
                    {"name": "leveldb::Visit", "edges": [
                        {"symbol": {"name": "leveldb::Slice::remove_prefix"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-library-type-receiver.test.ts:356 — a member of another class in the file
#[tokio::test]
async fn cpp_library_type_receiver_case_356_a_member_of_another_class_in_the_file() {
    check_case(Case {
        name: "cpp_library_type_receiver_case_356_a_member_of_another_class_in_the_file",
        files: vec![
            SourceFile {
                path: "include/leveldb/slice.h",
                source: "namespace leveldb {\nclass Slice {\n public:\n  const char* data() const { return data_; }\n  unsigned size() const { return size_; }\n  void clear() { size_ = 0; }\n  const char* begin() const { return data_; }\n  void remove_prefix(unsigned n) { size_ -= n; }\n private:\n  const char* data_;\n  unsigned size_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/buffers.h",
                source: "namespace leveldb {\nclass LeftoverBuffer {\n public:\n  void append(const char* s, unsigned n) {}\n};\nclass Arena {\n public:\n  void insert(int block) {}\n};\nclass Cache {\n public:\n  void reset() {}\n};\nclass Iterator {\n public:\n  void SeekToFirst() {}\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/log_test.cc",
                source: "#include <string>\n#include \"include/leveldb/slice.h\"\nnamespace leveldb {\nclass StringDest {\n public:\n  std::string contents_;\n};\nclass StringSource {\n public:\n  void Skip(unsigned n) { contents_.remove_prefix(n); }\n private:\n  Slice contents_;\n};\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::StringSource::Skip",
                expected: json!([
                    {"name": "leveldb::StringSource::Skip", "edges": [
                        {"symbol": {"name": "leveldb::Slice::remove_prefix"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-library-type-receiver.test.ts:385 — the last line of a declaration that starts on an earlier one
#[tokio::test]
async fn cpp_library_type_receiver_case_385_the_last_line_of_a_declaration_that_starts_on_an_earlier_one()
 {
    check_case(Case {
        name: "cpp_library_type_receiver_case_385_the_last_line_of_a_declaration_that_starts_on_an_earlier_one",
        files: vec![
            SourceFile {
                path: "util/autovector.h",
                source: "namespace rocksdb {\ntemplate <class T>\nclass autovector {\n public:\n  unsigned size() const { return 0; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/blob/blob_file_reader.cc",
                source: "#include <memory>\n#include \"util/autovector.h\"\nnamespace rocksdb {\nclass BlobFileReader {\n public:\n  unsigned MultiGetBlob(autovector<std::pair<int*,\n                                 std::unique_ptr<int>>>& blob_reqs);\n};\nunsigned BlobFileReader::MultiGetBlob(autovector<std::pair<int*,\n                                      std::unique_ptr<int>>>& blob_reqs) {\n  return blob_reqs.size();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::BlobFileReader::MultiGetBlob",
                expected: json!([
                    {"name": "rocksdb::BlobFileReader::MultiGetBlob", "edges": [
                        {"symbol": {"name": "rocksdb::autovector::size"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-library-type-receiver.test.ts:421 — a member of a class nested in the caller's
#[tokio::test]
async fn cpp_library_type_receiver_case_421_a_member_of_a_class_nested_in_the_caller_s() {
    check_case(Case {
        name: "cpp_library_type_receiver_case_421_a_member_of_a_class_nested_in_the_caller_s",
        files: vec![
            SourceFile {
                path: "include/rocksdb/multi_scan.h",
                source: "#include <vector>\nnamespace rocksdb {\nstruct ScanOptions {};\nclass MultiScanArgs {\n public:\n  int GetScanRanges() const { return 0; }\n};\nclass MultiScan {\n public:\n  class MultiScanIterator {\n   private:\n    const std::vector<ScanOptions>& scan_opts_;\n  };\n  int begin() { return scan_opts_.GetScanRanges(); }\n private:\n  const MultiScanArgs scan_opts_;\n};\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::MultiScan::begin",
                expected: json!([
                    {"name": "rocksdb::MultiScan::begin", "edges": [
                        {"symbol": {"name": "rocksdb::MultiScanArgs::GetScanRanges"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-library-type-receiver.test.ts:455 — a comment that mentions the receiver is not its declaration
#[tokio::test]
async fn cpp_library_type_receiver_case_455_a_comment_that_mentions_the_receiver_is_not_its_declaration()
 {
    check_case(Case {
        name: "cpp_library_type_receiver_case_455_a_comment_that_mentions_the_receiver_is_not_its_declaration",
        files: vec![
            SourceFile {
                path: "db/memtable.h",
                source: "namespace leveldb {\nclass MemTable {\n public:\n  unsigned ApproximateMemoryUsage() { return 0; }\n};\nclass Arena {\n public:\n  unsigned ApproximateMemoryUsage() { return 0; }\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/db_impl.h",
                source: "#include \"db/memtable.h\"\nnamespace leveldb {\nclass DBImpl {\n public:\n  unsigned MemoryUsage();\n private:\n  MemTable* imm_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/db_impl.cc",
                source: "#include \"db/db_impl.h\"\nnamespace leveldb {\nunsigned DBImpl::MemoryUsage() {\n  // Count the memtable being compacted, since there is a non-null imm_\n  return imm_->ApproximateMemoryUsage();\n}\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::DBImpl::MemoryUsage",
                expected: json!([
                    {"name": "leveldb::DBImpl::MemoryUsage", "edges": [
                        {"symbol": {"name": "leveldb::MemTable::ApproximateMemoryUsage"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-outside-type-receiver.test.ts:89 — reproduction: `rocksdb::DB* db
// db->Put(…)` is rocksdb's own DB
#[tokio::test]
async fn cpp_outside_type_receiver_case_89_reproduction_rocksdb_db_db_db_put_is_rocksdb_s_own_db() {
    check_case(Case {
        name: "cpp_outside_type_receiver_case_89_reproduction_rocksdb_db_db_db_put_is_rocksdb_s_own_db",
        files: vec![
            SourceFile {
                path: "include/rocksdb/rocksdb_namespace.h",
                source: "#ifndef ROCKSDB_NAMESPACE\n#define ROCKSDB_NAMESPACE rocksdb\n#endif\n",
            },
            SourceFile {
                path: "include/rocksdb/db.h",
                source: "#include \"include/rocksdb/rocksdb_namespace.h\"\nnamespace ROCKSDB_NAMESPACE {\nclass DB {\n public:\n  int Put(int key) { return key; }\n};\n}  // namespace ROCKSDB_NAMESPACE\n",
            },
            SourceFile {
                path: "upb/status.h",
                source: "namespace upb {\nclass Status {\n public:\n  bool ok() const { return true; }\n};\n}  // namespace upb\n",
            },
            SourceFile {
                path: "java/rocksjni/put.cc",
                source: "#include \"include/rocksdb/db.h\"\nint Put(rocksdb::DB* db) {\n  return db->Put(1);\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Put",
                expected: json!([
                    {"name": "Put", "edges": [
                        {"symbol": {"name": "ROCKSDB_NAMESPACE::DB::Put"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-outside-type-receiver.test.ts:106 — under either name, while a namespace the project declares nothing in stays outside
#[tokio::test]
async fn cpp_outside_type_receiver_case_106_under_either_name_while_a_namespace_the_project_declares_nothing_in_stays_o()
 {
    check_case(Case {
        name: "cpp_outside_type_receiver_case_106_under_either_name_while_a_namespace_the_project_declares_nothing_in_stays_o",
        files: vec![
            SourceFile {
                path: "include/rocksdb/rocksdb_namespace.h",
                source: "#ifndef ROCKSDB_NAMESPACE\n#define ROCKSDB_NAMESPACE rocksdb\n#endif\n",
            },
            SourceFile {
                path: "upb/status.h",
                source: "namespace upb {\nclass Status {\n public:\n  bool ok() const { return true; }\n};\n}  // namespace upb\n",
            },
            SourceFile {
                path: "cache/cache.h",
                source: "namespace rocksdb {\nclass Cache {\n public:\n  void Release(int handle) {}\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/release.cc",
                source: "#include \"include/rocksdb/rocksdb_namespace.h\"\n#include \"cache/cache.h\"\n#include \"upb/status.h\"\nvoid Drop(ROCKSDB_NAMESPACE::Cache* cache) {\n  cache->Release(1);\n}\nbool Passed(const absl::Status& status) {\n  return status.ok();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Drop",
                expected: json!([
                    {"name": "Drop", "edges": [
                        {"symbol": {"name": "rocksdb::Cache::Release"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Passed",
                expected: json!([
                    {"name": "Passed", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-outside-type-receiver.test.ts:154 — reproduction: `absl::flat_hash_set<int> seen
// seen.insert(…)` is not util::flat_hash_set's
#[tokio::test]
async fn cpp_outside_type_receiver_case_154_reproduction_absl_flat_hash_set_int_seen_seen_insert_is_not_util_flat_hash()
 {
    check_case(Case {
        name: "cpp_outside_type_receiver_case_154_reproduction_absl_flat_hash_set_int_seen_seen_insert_is_not_util_flat_hash",
        files: vec![
            SourceFile {
                path: "util/hash_set.h",
                source: "namespace util {\ntemplate <class T>\nclass flat_hash_set {\n public:\n  void insert(const T& v) {}\n};\n}  // namespace util\n",
            },
            SourceFile {
                path: "db/seen.cc",
                source: "#include \"absl/container/flat_hash_set.h\"\n#include \"util/hash_set.h\"\nvoid Mark(int v) {\n  absl::flat_hash_set<int> seen;\n  seen.insert(v);\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Mark",
                expected: json!([
                    {"name": "Mark", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "util::flat_hash_set::insert",
                expected: json!([
                    {"name": "util::flat_hash_set::insert", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-outside-type-receiver.test.ts:174 — the project's own, written with its namespace, a namespace alias, or one a macro opens
#[tokio::test]
async fn cpp_outside_type_receiver_case_174_the_project_s_own_written_with_its_namespace_a_namespace_alias_or_one_a_mac()
 {
    check_case(Case {
        name: "cpp_outside_type_receiver_case_174_the_project_s_own_written_with_its_namespace_a_namespace_alias_or_one_a_mac",
        files: vec![
            SourceFile {
                path: "util/hash_set.h",
                source: "namespace util {\ntemplate <class T>\nclass flat_hash_set {\n public:\n  void insert(const T& v) {}\n};\n}  // namespace util\n",
            },
            SourceFile {
                path: "include/fmt/format.h",
                source: "#define FMT_BEGIN_NAMESPACE namespace fmt { inline namespace v11 {\n#define FMT_END_NAMESPACE } }\nFMT_BEGIN_NAMESPACE\nnamespace detail {\ntemplate <typename T>\nclass buffer {\n public:\n  void push_back(const T& value) {}\n};\n}  // namespace detail\nFMT_END_NAMESPACE\n",
            },
            SourceFile {
                path: "other/buffer.h",
                source: "namespace other {\nclass buffer {\n public:\n  void push_back(int value) {}\n};\n}  // namespace other\n",
            },
            SourceFile {
                path: "db/keep.cc",
                source: "#include \"util/hash_set.h\"\n#include \"include/fmt/format.h\"\nnamespace u = util;\nvoid Keep(int v) {\n  util::flat_hash_set<int> mine;\n  mine.insert(v);\n}\nvoid Alias(int v) {\n  u::flat_hash_set<int> aliased;\n  aliased.insert(v);\n}\nvoid Append(fmt::detail::buffer<char>& buf) {\n  buf.push_back('x');\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Keep",
                expected: json!([
                    {"name": "Keep", "edges": [
                        {"symbol": {"name": "util::flat_hash_set::insert"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Alias",
                expected: json!([
                    {"name": "Alias", "edges": [
                        {"symbol": {"name": "util::flat_hash_set::insert"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Append",
                expected: json!([
                    {"name": "Append", "edges": [
                        {"symbol": {"name": "detail::buffer::push_back"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-outside-type-receiver.test.ts:264 — reproduction: a generated `DescriptorProto::ExtensionRange` is not `Descriptor::ExtensionRange`
#[tokio::test]
async fn cpp_outside_type_receiver_case_264_reproduction_a_generated_descriptorproto_extensionrange_is_not_descriptor_e()
 {
    check_case(Case {
        name: "cpp_outside_type_receiver_case_264_reproduction_a_generated_descriptorproto_extensionrange_is_not_descriptor_e",
        files: vec![
            SourceFile {
                path: "src/google/protobuf/descriptor.h",
                source: "namespace google {\nnamespace protobuf {\nclass Descriptor {\n public:\n  class ExtensionRange {\n   public:\n    const int& options() const { return options_; }\n   private:\n    int options_;\n  };\n};\nnamespace util {\nclass SimpleFieldComparator {\n public:\n  void set_treat_nan_as_equal(bool value) {}\n};\nclass DefaultFieldComparator : public SimpleFieldComparator {};\n}  // namespace util\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/google/protobuf/descriptor.pb.cc",
                source: "namespace google {\nnamespace protobuf {\nclass DescriptorProto::_Internal {\n public:\n  using HasBits = int;\n};\nDescriptorProto::DescriptorProto() {}\nvoid DescriptorProto::Clear() {}\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/google/protobuf/compiler/importer_unittest.cc",
                source: "#include \"src/google/protobuf/descriptor.h\"\nnamespace google {\nnamespace protobuf {\nnamespace compiler {\nint Options(const DescriptorProto::ExtensionRange& range) {\n  return range.options();\n}\n}  // namespace compiler\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::compiler::Options",
                expected: json!([
                    {"name": "google::protobuf::compiler::Options", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-outside-type-receiver.test.ts:287 — a class whose methods alone are indexed, a base class's method, a template parameter's member
#[tokio::test]
async fn cpp_outside_type_receiver_case_287_a_class_whose_methods_alone_are_indexed_a_base_class_s_method_a_template_pa()
 {
    check_case(Case {
        name: "cpp_outside_type_receiver_case_287_a_class_whose_methods_alone_are_indexed_a_base_class_s_method_a_template_pa",
        files: vec![
            SourceFile {
                path: "src/google/protobuf/descriptor.h",
                source: "namespace google {\nnamespace protobuf {\nclass Descriptor {\n public:\n  class ExtensionRange {\n   public:\n    const int& options() const { return options_; }\n   private:\n    int options_;\n  };\n};\nnamespace util {\nclass SimpleFieldComparator {\n public:\n  void set_treat_nan_as_equal(bool value) {}\n};\nclass DefaultFieldComparator : public SimpleFieldComparator {};\n}  // namespace util\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/google/protobuf/descriptor.pb.cc",
                source: "namespace google {\nnamespace protobuf {\nclass DescriptorProto::_Internal {\n public:\n  using HasBits = int;\n};\nDescriptorProto::DescriptorProto() {}\nvoid DescriptorProto::Clear() {}\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "upb/reflection/descriptor_proto.h",
                source: "namespace upb {\nclass DescriptorProto {};\n}  // namespace upb\n",
            },
            SourceFile {
                path: "hpb/iterator.h",
                source: "namespace hpb {\nnamespace internal {\nstruct ScalarIteratorPolicy {\n  struct Payload {\n    void AddOffset(int n) {}\n  };\n};\ntemplate <typename PolicyT>\nclass Iterator {\n private:\n  typename PolicyT::Payload it_;\n public:\n  void Next() { it_.AddOffset(1); }\n};\n}  // namespace internal\n}  // namespace hpb\n",
            },
            SourceFile {
                path: "src/google/protobuf/compiler/reset.cc",
                source: "#include \"src/google/protobuf/descriptor.h\"\nnamespace google {\nnamespace protobuf {\nnamespace compiler {\nvoid Reset(google::protobuf::DescriptorProto* proto) {\n  proto->Clear();\n}\nvoid Compare() {\n  google::protobuf::util::DefaultFieldComparator comparator;\n  comparator.set_treat_nan_as_equal(true);\n}\n}  // namespace compiler\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::compiler::Reset",
                expected: json!([
                    {"name": "google::protobuf::compiler::Reset", "edges": [
                        {"symbol": {"name": "google::protobuf::DescriptorProto::Clear"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::compiler::Compare",
                expected: json!([
                    {"name": "google::protobuf::compiler::Compare", "edges": [
                        {"symbol": {"name": "google::protobuf::util::SimpleFieldComparator::set_treat_nan_as_equal"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "hpb::internal::Iterator::Next",
                expected: json!([
                    {"name": "hpb::internal::Iterator::Next", "edges": [
                        {"symbol": {"name": "hpb::internal::ScalarIteratorPolicy::Payload::AddOffset"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-outside-type-receiver.test.ts:340 — a nested type a derived class inherits, named through the derived class
#[tokio::test]
async fn cpp_outside_type_receiver_case_340_a_nested_type_a_derived_class_inherits_named_through_the_derived_class()
 {
    check_case(Case {
        name: "cpp_outside_type_receiver_case_340_a_nested_type_a_derived_class_inherits_named_through_the_derived_class",
        files: vec![
            SourceFile {
                path: "src/tree.h",
                source: "namespace ns {\nstruct Base {\n  struct Node {\n    void Visit() {}\n  };\n};\nstruct Derived : Base {};\n}  // namespace ns\n",
            },
            SourceFile {
                path: "src/walk.cc",
                source: "#include \"src/tree.h\"\nvoid Root() {\n  ns::Derived::Node node;\n}\nvoid Walk() {\n  node.Visit();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Walk",
                expected: json!([
                    {"name": "Walk", "edges": [
                        {"symbol": {"name": "ns::Base::Node::Visit"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-outside-type-receiver.test.ts:371 — an outside type no project class shares a name with stays past guessing
#[tokio::test]
async fn cpp_outside_type_receiver_case_371_an_outside_type_no_project_class_shares_a_name_with_stays_past_guessing()
 {
    check_case(Case {
        name: "cpp_outside_type_receiver_case_371_an_outside_type_no_project_class_shares_a_name_with_stays_past_guessing",
        files: vec![
            SourceFile {
                path: "src/stream.h",
                source: "class Stream {\n public:\n  bool Read(int n) { return n > 0; }\n};\n",
            },
            SourceFile {
                path: "src/client.cc",
                source: "#include \"src/stream.h\"\nvoid Open() {\n  grpc::ClientReader<int> reader;\n}\nbool Pump() {\n  return reader->Read(1);\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Pump",
                expected: json!([
                    {"name": "Pump", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-outside-type-receiver.test.ts:398 — a `->` on an outside holder of a class the project shares a name with reaches what it holds
#[tokio::test]
async fn cpp_outside_type_receiver_case_398_a_on_an_outside_holder_of_a_class_the_project_shares_a_name_with_reaches_wh()
 {
    check_case(Case {
        name: "cpp_outside_type_receiver_case_398_a_on_an_outside_holder_of_a_class_the_project_shares_a_name_with_reaches_wh",
        files: vec![
            SourceFile {
                path: "util/optional.h",
                source: "namespace util {\ntemplate <typename T>\nclass Optional {\n public:\n  bool has_value() const { return true; }\n};\n}  // namespace util\n",
            },
            SourceFile {
                path: "src/widget.h",
                source: "class Widget {\n public:\n  void Draw() {}\n};\n",
            },
            SourceFile {
                path: "src/paint.cc",
                source: "#include \"util/optional.h\"\n#include \"src/widget.h\"\nvoid Load() {\n  folly::Optional<Widget> widget;\n}\nvoid Paint() {\n  widget->Draw();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Paint",
                expected: json!([
                    {"name": "Paint", "edges": [
                        {"symbol": {"name": "Widget::Draw"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-template-member-types.test.ts:81 — reproduction: `SkipList<Key, Comparator>::Iterator iter` is the skip list's iterator
#[tokio::test]
async fn cpp_template_member_types_case_81_reproduction_skiplist_key_comparator_iterator_iter_is_the_skip_list_s_itera()
 {
    check_case(Case {
        name: "cpp_template_member_types_case_81_reproduction_skiplist_key_comparator_iterator_iter_is_the_skip_list_s_itera",
        files: vec![
            SourceFile {
                path: "include/iterator.h",
                source: "namespace leveldb {\nclass Iterator {\n public:\n  virtual ~Iterator() {}\n  virtual bool Valid() const { return false; }\n  virtual void SeekToFirst() {}\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/skiplist.h",
                source: "#include \"include/iterator.h\"\nnamespace leveldb {\ntemplate <typename Key, class Comparator>\nclass SkipList {\n public:\n  class Iterator {\n   public:\n    explicit Iterator(const SkipList* list) : list_(list) {}\n    bool Valid() const { return true; }\n    void SeekToFirst() {}\n   private:\n    const SkipList* list_;\n  };\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/skiplist_test.cc",
                source: "#include \"db/skiplist.h\"\nnamespace leveldb {\ntypedef unsigned long Key;\nstruct Comparator {};\nvoid Empty() {\n  SkipList<Key, Comparator> list;\n  SkipList<Key, Comparator>::Iterator iter(&list);\n  if (!iter.Valid()) iter.SeekToFirst();\n}\nvoid Split() {\n  SkipList<Key, Comparator> list;\n  SkipList<Key, Comparator>::Iterator\n      iter(&list);\n  iter.Valid();\n}\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::Empty",
                expected: json!([
                    {"name": "leveldb::Empty", "edges": [
                        {"symbol": {"name": "leveldb::SkipList::Iterator::SeekToFirst"}},
                        {"symbol": {"name": "leveldb::SkipList::Iterator::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "leveldb::Split",
                expected: json!([
                    {"name": "leveldb::Split", "edges": [
                        {"symbol": {"name": "leveldb::SkipList::Iterator::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-template-member-types.test.ts:111 — written with `typename` in a template, and through a pointer
#[tokio::test]
async fn cpp_template_member_types_case_111_written_with_typename_in_a_template_and_through_a_pointer()
 {
    check_case(Case {
        name: "cpp_template_member_types_case_111_written_with_typename_in_a_template_and_through_a_pointer",
        files: vec![
            SourceFile {
                path: "include/iterator.h",
                source: "namespace leveldb {\nclass Iterator {\n public:\n  virtual ~Iterator() {}\n  virtual bool Valid() const { return false; }\n  virtual void SeekToFirst() {}\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/skiplist.h",
                source: "#include \"include/iterator.h\"\nnamespace leveldb {\ntemplate <typename Key, class Comparator>\nclass SkipList {\n public:\n  class Iterator {\n   public:\n    explicit Iterator(const SkipList* list) : list_(list) {}\n    bool Valid() const { return true; }\n    void SeekToFirst() {}\n   private:\n    const SkipList* list_;\n  };\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/concurrent_test.cc",
                source: "#include \"db/skiplist.h\"\nnamespace leveldb {\ntemplate <typename Key, class Comparator>\nclass ConcurrentTest {\n public:\n  void ReadStep() {\n    typename SkipList<Key, Comparator>::Iterator iter(&list_);\n    iter.SeekToFirst();\n  }\n  bool Check(typename SkipList<Key, Comparator>::Iterator* it) {\n    return it->Valid();\n  }\n private:\n  SkipList<Key, Comparator> list_;\n};\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::ConcurrentTest::ReadStep",
                expected: json!([
                    {"name": "leveldb::ConcurrentTest::ReadStep", "edges": [
                        {"symbol": {"name": "leveldb::SkipList::Iterator::SeekToFirst"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "leveldb::ConcurrentTest::Check",
                expected: json!([
                    {"name": "leveldb::ConcurrentTest::Check", "edges": [
                        {"symbol": {"name": "leveldb::SkipList::Iterator::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-template-member-types.test.ts:141 — a standard container's iterator is the library's, not a project class named `iterator`
#[tokio::test]
async fn cpp_template_member_types_case_141_a_standard_container_s_iterator_is_the_library_s_not_a_project_class_named()
 {
    check_case(Case {
        name: "cpp_template_member_types_case_141_a_standard_container_s_iterator_is_the_library_s_not_a_project_class_named",
        files: vec![
            SourceFile {
                path: "util/ring.h",
                source: "class Ring {\n public:\n  class iterator {\n   public:\n    int base() const { return 0; }\n  };\n};\n",
            },
            SourceFile {
                path: "src/touch.cc",
                source: "#include <vector>\n#include \"util/ring.h\"\nvoid Touch(std::vector<int>::iterator it) {\n  it.base();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Touch",
                expected: json!([
                    {"name": "Touch", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-template-member-types.test.ts:167 — a library template's member that names another type may be one of the project's
#[tokio::test]
async fn cpp_template_member_types_case_167_a_library_template_s_member_that_names_another_type_may_be_one_of_the_proje()
 {
    check_case(Case {
        name: "cpp_template_member_types_case_167_a_library_template_s_member_that_names_another_type_may_be_one_of_the_proje",
        files: vec![
            SourceFile {
                path: "include/slice.h",
                source: "class Slice {\n public:\n  int size() const { return 0; }\n};\n",
            },
            SourceFile {
                path: "src/keys.cc",
                source: "#include <vector>\n#include \"include/slice.h\"\nint First(const std::vector<Slice>& keys) {\n  std::vector<Slice>::value_type key = keys[0];\n  return key.size();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "First",
                expected: json!([
                    {"name": "First", "edges": [
                        {"symbol": {"name": "Slice::size"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-template-member-types.test.ts:191 — a member alias of a class template is the class it names
#[tokio::test]
async fn cpp_template_member_types_case_191_a_member_alias_of_a_class_template_is_the_class_it_names()
 {
    check_case(Case {
        name: "cpp_template_member_types_case_191_a_member_alias_of_a_class_template_is_the_class_it_names",
        files: vec![
            SourceFile {
                path: "cache/clock_cache.h",
                source: "class Shard {\n public:\n  int GetTable() const { return 0; }\n};\nnamespace cache {\ntemplate <class Table>\nclass ClockCacheShard {\n public:\n  int GetTable() const { return 1; }\n};\ntemplate <class Table>\nclass BaseHyperClockCache {\n public:\n  using Shard = ClockCacheShard<Table>;\n  int Report(const BaseHyperClockCache<Table>::Shard* shard) const {\n    return shard->GetTable();\n  }\n};\n}  // namespace cache\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "cache::BaseHyperClockCache::Report",
                expected: json!([
                    {"name": "cache::BaseHyperClockCache::Report", "edges": [
                        {"symbol": {"name": "cache::ClockCacheShard::GetTable"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-iterator-arrow.test.ts:48 — reproduction: `std::deque<Slice>::const_iterator it
// it->data()` calls the element's method
#[tokio::test]
async fn cpp_iterator_arrow_case_48_reproduction_std_deque_slice_const_iterator_it_it_data_calls_the_element_s()
 {
    check_case(Case {
        name: "cpp_iterator_arrow_case_48_reproduction_std_deque_slice_const_iterator_it_it_data_calls_the_element_s",
        files: vec![
            SourceFile {
                path: "util/buffer.h",
                source: "class Buffer {\n public:\n  const char* data() const { return nullptr; }\n};\n",
            },
            SourceFile {
                path: "include/slice.h",
                source: "class Slice {\n public:\n  const char* data() const { return nullptr; }\n};\n",
            },
            SourceFile {
                path: "util/append.cc",
                source: "#include <deque>\n#include \"util/buffer.h\"\n#include \"include/slice.h\"\nvoid Append(const std::deque<Slice>& operands) {\n  for (std::deque<Slice>::const_iterator it = operands.begin(); it != operands.end(); ++it) {\n    it->data();\n  }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Append",
                expected: json!([
                    {"name": "Append", "edges": [
                        {"symbol": {"name": "Slice::data"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-iterator-arrow.test.ts:80 — an element of a library type has no project method to call
#[tokio::test]
async fn cpp_iterator_arrow_case_80_an_element_of_a_library_type_has_no_project_method_to_call() {
    check_case(Case {
        name: "cpp_iterator_arrow_case_80_an_element_of_a_library_type_has_no_project_method_to_call",
        files: vec![
            SourceFile {
                path: "util/name.h",
                source: "class Name {\n public:\n  const char* c_str() const { return nullptr; }\n};\n",
            },
            SourceFile {
                path: "tools/print.cc",
                source: "#include <string>\n#include <vector>\n#include \"util/name.h\"\nvoid Print(const std::vector<std::string>& names) {\n  for (std::vector<std::string>::const_iterator itr = names.begin(); itr != names.end(); ++itr) {\n    itr->c_str();\n  }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Print",
                expected: json!([
                    {"name": "Print", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-iterator-arrow.test.ts:106 — the iterator a project container's own alias names: protobuf's `RepeatedPtrField<T>::iterator`
#[tokio::test]
async fn cpp_iterator_arrow_case_106_the_iterator_a_project_container_s_own_alias_names_protobuf_s_repeatedptrfi()
 {
    check_case(Case {
        name: "cpp_iterator_arrow_case_106_the_iterator_a_project_container_s_own_alias_names_protobuf_s_repeatedptrfi",
        files: vec![
            SourceFile {
                path: "src/repeated_ptr_field.h",
                source: "namespace google {\nnamespace protobuf {\nnamespace internal {\ntemplate <typename Element>\nclass RepeatedPtrIterator {\n public:\n  Element& operator*() const { return *it_; }\n  Element* operator->() const { return it_; }\n private:\n  Element* it_;\n};\n}  // namespace internal\ntemplate <typename Element>\nclass RepeatedPtrField {\n public:\n  typedef internal::RepeatedPtrIterator<Element> iterator;\n  iterator begin() { return iterator(); }\n};\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/descriptor.pb.h",
                source: "namespace google {\nnamespace protobuf {\nclass FileDescriptorProto {\n public:\n  int path_size() const { return 1; }\n};\nclass SourceCodeInfo_Location {\n public:\n  int path_size() const { return 0; }\n};\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/option_interpreter.cc",
                source: "#include \"src/repeated_ptr_field.h\"\n#include \"src/descriptor.pb.h\"\nnamespace google {\nnamespace protobuf {\nint Count(RepeatedPtrField<SourceCodeInfo_Location>* locs) {\n  RepeatedPtrField<SourceCodeInfo_Location>::iterator loc = locs->begin();\n  return loc->path_size();\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::Count",
                expected: json!([
                    {"name": "google::protobuf::Count", "edges": [
                        {"symbol": {"name": "google::protobuf::RepeatedPtrField::begin"}},
                        {"symbol": {"name": "google::protobuf::SourceCodeInfo_Location::path_size"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-iterator-arrow.test.ts:167 — an element of the calling template's own parameter names no particular class
#[tokio::test]
async fn cpp_iterator_arrow_case_167_an_element_of_the_calling_template_s_own_parameter_names_no_particular_clas()
 {
    check_case(Case {
        name: "cpp_iterator_arrow_case_167_an_element_of_the_calling_template_s_own_parameter_names_no_particular_clas",
        files: vec![
            SourceFile {
                path: "src/visit.cc",
                source: "#include <vector>\nclass Node {\n public:\n  void Visit() {}\n};\ntemplate <typename T>\nvoid VisitAll(typename std::vector<T>::iterator it) {\n  it->Visit();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "VisitAll",
                expected: json!([
                    {"name": "VisitAll", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-iterator-arrow.test.ts:188 — an element only a class template's own `operator*` names goes on as before
#[tokio::test]
async fn cpp_iterator_arrow_case_188_an_element_only_a_class_template_s_own_operator_names_goes_on_as_before()
 {
    check_case(Case {
        name: "cpp_iterator_arrow_case_188_an_element_only_a_class_template_s_own_operator_names_goes_on_as_before",
        files: vec![
            SourceFile {
                path: "gtest/param.h",
                source: "template <typename T>\nclass ParamIterator {\n public:\n  const T& operator*() const { return *value_; }\n  const T* operator->() const { return value_; }\n private:\n  const T* value_;\n};\ntemplate <typename T>\nclass ParamGenerator {\n public:\n  typedef ParamIterator<T> iterator;\n  iterator begin() const { return iterator(); }\n};\n",
            },
            SourceFile {
                path: "gtest/run.cc",
                source: "#include \"gtest/param.h\"\nclass Case {\n public:\n  void Run() const {}\n};\nvoid RunAll(const ParamGenerator<Case>& gen) {\n  ParamGenerator<Case>::iterator it = gen.begin();\n  it->Run();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "RunAll",
                expected: json!([
                    {"name": "RunAll", "edges": [
                        {"symbol": {"name": "Case::Run"}},
                        {"symbol": {"name": "ParamGenerator::begin"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-global-alias-target.test.ts:65 — reproduction: protobuf hpb's `backend::Arena` is `upb::Arena`, not the calling method's own class
#[tokio::test]
async fn cpp_global_alias_target_case_65_reproduction_protobuf_hpb_s_backend_arena_is_upb_arena_not_the_calling_meth()
 {
    check_case(Case {
        name: "cpp_global_alias_target_case_65_reproduction_protobuf_hpb_s_backend_arena_is_upb_arena_not_the_calling_meth",
        files: vec![
            SourceFile {
                path: "upb/mem/arena.hpp",
                source: "namespace upb {\nclass Arena {\n public:\n  bool Fuse(Arena& other) { return true; }\n  bool IsFused(Arena& other) const { return false; }\n  void RefArena(const Arena& to) {}\n};\n}  // namespace upb\n",
            },
            SourceFile {
                path: "hpb/multibackend.h",
                source: "#define HPB_INTERNAL_BACKEND_UPB 1\n#define HPB_INTERNAL_BACKEND_CPP 2\nnamespace hpb {\nnamespace internal {\nnamespace backend {\nnamespace upb {}\nnamespace cpp {}\n}  // namespace backend\n}  // namespace internal\n#if HPB_INTERNAL_BACKEND == HPB_INTERNAL_BACKEND_UPB\nnamespace backend = hpb::internal::backend::upb;\n#elif HPB_INTERNAL_BACKEND == HPB_INTERNAL_BACKEND_CPP\nnamespace backend = hpb::internal::backend::cpp;\n#endif\n}  // namespace hpb\n",
            },
            SourceFile {
                path: "hpb/backend/types.h",
                source: "#include \"hpb/multibackend.h\"\n#include \"upb/mem/arena.hpp\"\nnamespace hpb {\nnamespace internal {\nnamespace backend {\n#if HPB_INTERNAL_BACKEND == HPB_INTERNAL_BACKEND_UPB\nnamespace upb {\nusing Arena = ::upb::Arena;\n}\n#elif HPB_INTERNAL_BACKEND == HPB_INTERNAL_BACKEND_CPP\nnamespace cpp {\nusing Arena = google::protobuf::Arena;\n}\n#endif\n}  // namespace backend\n}  // namespace internal\n}  // namespace hpb\n",
            },
            SourceFile {
                path: "hpb/arena.h",
                source: "#include \"hpb/backend/types.h\"\nnamespace hpb {\nclass Arena {\n public:\n  bool Fuse(Arena& other) { return arena_.Fuse(other.arena_); };\n  bool IsFused(Arena& other) { return arena_.IsFused(other.arena_); };\n  void RefArena(const Arena& to) { arena_.RefArena(to.arena_); }\n private:\n  backend::Arena arena_;\n};\n}  // namespace hpb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "hpb::Arena::Fuse",
                expected: json!([
                    {"name": "hpb::Arena::Fuse", "edges": [
                        {"symbol": {"name": "upb::Arena::Fuse"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "hpb::Arena::IsFused",
                expected: json!([
                    {"name": "hpb::Arena::IsFused", "edges": [
                        {"symbol": {"name": "upb::Arena::IsFused"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "hpb::Arena::RefArena",
                expected: json!([
                    {"name": "hpb::Arena::RefArena", "edges": [
                        {"symbol": {"name": "upb::Arena::RefArena"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-global-alias-target.test.ts:127 — names the global class, not a namesake in the scope around the alias
#[tokio::test]
async fn cpp_global_alias_target_case_127_names_the_global_class_not_a_namesake_in_the_scope_around_the_alias()
 {
    check_case(Case {
        name: "cpp_global_alias_target_case_127_names_the_global_class_not_a_namesake_in_the_scope_around_the_alias",
        files: vec![
            SourceFile {
                path: "upb/mem/arena.hpp",
                source: "namespace upb {\nclass Arena {\n public:\n  bool Fuse(Arena& other) { return true; }\n  bool IsFused(Arena& other) const { return false; }\n  void RefArena(const Arena& to) {}\n};\n}  // namespace upb\n",
            },
            SourceFile {
                path: "hpb/arena.cc",
                source: "#include \"upb/mem/arena.hpp\"\nnamespace hpb {\nnamespace upb {\nclass Arena {\n public:\n  bool Fuse(Arena& other) { return false; }\n};\n}  // namespace upb\nusing GlobalArena = ::upb::Arena;\nbool Join(GlobalArena& a, GlobalArena& b) {\n  return a.Fuse(b);\n}\n}  // namespace hpb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "hpb::Join",
                expected: json!([
                    {"name": "hpb::Join", "edges": [
                        {"symbol": {"name": "upb::Arena::Fuse"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-global-alias-target.test.ts:153 — through an `auto` variable the alias types
#[tokio::test]
async fn cpp_global_alias_target_case_153_through_an_auto_variable_the_alias_types() {
    check_case(Case {
        name: "cpp_global_alias_target_case_153_through_an_auto_variable_the_alias_types",
        files: vec![
            SourceFile {
                path: "upb/mem/arena.hpp",
                source: "namespace upb {\nclass Arena {\n public:\n  bool Fuse(Arena& other) { return true; }\n  bool IsFused(Arena& other) const { return false; }\n  void RefArena(const Arena& to) {}\n};\n}  // namespace upb\n",
            },
            SourceFile {
                path: "hpb/arena.cc",
                source: "#include \"upb/mem/arena.hpp\"\nnamespace hpb {\nnamespace upb {\nusing Arena = ::upb::Arena;\n}  // namespace upb\nclass Arena {\n public:\n  bool Fuse(Arena& other) { return false; }\n};\nupb::Arena MakeArena() { return upb::Arena(); }\nbool Join(upb::Arena& other) {\n  auto made = MakeArena();\n  return made.Fuse(other);\n}\n}  // namespace hpb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "hpb::Join",
                expected: json!([
                    {"name": "hpb::Join", "edges": [
                        {"symbol": {"name": "upb::Arena::Fuse"}},
                    ]},
                ]),
                endpoint_suffix: Some("::Fuse"),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-global-alias-target.test.ts:183 — a base class: `::lib::Options`, not the namespace's own `lib::Options`
#[tokio::test]
async fn cpp_global_alias_target_case_183_a_base_class_lib_options_not_the_namespace_s_own_lib_options()
 {
    check_case(Case {
        name: "cpp_global_alias_target_case_183_a_base_class_lib_options_not_the_namespace_s_own_lib_options",
        files: vec![
            SourceFile {
                path: "src/config.cc",
                source: "namespace app {\nstruct Inner {\n  void Run() {}\n};\n}  // namespace app\nnamespace lib {\nstruct Options {\n  struct Inner {\n    void Run() {}\n  };\n};\n}  // namespace lib\nnamespace app {\nnamespace lib {\nstruct Options {};\n}  // namespace lib\nclass Config : public ::lib::Options {};\nvoid Use() {\n  Config::Inner inner;\n  inner.Run();\n}\n}  // namespace app\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "app::Use",
                expected: json!([
                    {"name": "app::Use", "edges": [
                        {"symbol": {"name": "lib::Options::Inner::Run"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-smart-pointer-receiver.test.ts:124 — reproduction: `std::unique_ptr<Iterator> iter
// iter->Valid()` is the Iterator the scope sees, not a namesake
#[tokio::test]
async fn cpp_smart_pointer_receiver_case_124_reproduction_std_unique_ptr_iterator_iter_iter_valid_is_the_iterator_the_sc()
 {
    check_case(Case {
        name: "cpp_smart_pointer_receiver_case_124_reproduction_std_unique_ptr_iterator_iter_iter_valid_is_the_iterator_the_sc",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator_base.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual ~IteratorBase() {}\n  virtual bool Valid() const = 0;\n  virtual void Next() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "#include \"include/rocksdb/iterator_base.h\"\nnamespace rocksdb {\nclass Iterator : public IteratorBase {\n public:\n  virtual int value() const = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/memtablerep.h",
                source: "namespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    virtual bool Valid() const = 0;\n    virtual void Next() = 0;\n  };\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/arena_wrapped_db_iter.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass ArenaWrappedDBIter : public Iterator {\n public:\n  bool Valid() const override { return true; }\n  void Next() override {}\n  int value() const override { return 0; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/db.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DB {\n public:\n  virtual int Get(int key) = 0;\n  virtual Iterator* NewIterator() = 0;\n};\nclass DBGetBatch {\n public:\n  int Get(int key) { return key; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_impl.cc",
                source: "#include <memory>\n#include \"include/rocksdb/db.h\"\n#include \"db/arena_wrapped_db_iter.h\"\nnamespace rocksdb {\nint CountKeys(DB* db) {\n  std::unique_ptr<Iterator> iter(db->NewIterator());\n  int n = 0;\n  for (; iter->Valid(); iter->Next()) n += iter->value();\n  return n;\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::CountKeys",
                expected: json!([
                    {"name": "rocksdb::CountKeys", "edges": [
                        {"symbol": {"name": "rocksdb::DB::NewIterator"}},
                        {"symbol": {"name": "rocksdb::Iterator::value"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::Next"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-smart-pointer-receiver.test.ts:154 — `std::shared_ptr`, `std::optional`, a parameter, and a member the class declares in its header
#[tokio::test]
async fn cpp_smart_pointer_receiver_case_154_std_shared_ptr_std_optional_a_parameter_and_a_member_the_class_declares_in()
 {
    check_case(Case {
        name: "cpp_smart_pointer_receiver_case_154_std_shared_ptr_std_optional_a_parameter_and_a_member_the_class_declares_in",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator_base.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual ~IteratorBase() {}\n  virtual bool Valid() const = 0;\n  virtual void Next() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "#include \"include/rocksdb/iterator_base.h\"\nnamespace rocksdb {\nclass Iterator : public IteratorBase {\n public:\n  virtual int value() const = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/memtablerep.h",
                source: "namespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    virtual bool Valid() const = 0;\n    virtual void Next() = 0;\n  };\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/arena_wrapped_db_iter.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass ArenaWrappedDBIter : public Iterator {\n public:\n  bool Valid() const override { return true; }\n  void Next() override {}\n  int value() const override { return 0; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/db.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DB {\n public:\n  virtual int Get(int key) = 0;\n  virtual Iterator* NewIterator() = 0;\n};\nclass DBGetBatch {\n public:\n  int Get(int key) { return key; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "table/table_db.h",
                source: "namespace rocksdb {\nclass TableDB {\n public:\n  int Get(int key) { return key; }\n};\nclass CuckooTableDB {\n public:\n  int Get(int key) { return key; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_impl.h",
                source: "#include <memory>\n#include \"include/rocksdb/db.h\"\nnamespace rocksdb {\nclass DBImpl {\n public:\n  int Lookup(int key);\n private:\n  std::shared_ptr<DB> table_db_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_impl.cc",
                source: "#include <optional>\n#include \"db/db_impl.h\"\n#include \"table/table_db.h\"\nnamespace rocksdb {\nint DBImpl::Lookup(int key) {\n  return table_db_->Get(key);\n}\nint Probe(const std::shared_ptr<DB>& cuckoo_db) {\n  std::optional<DBGetBatch> batch;\n  return cuckoo_db->Get(1) + batch->Get(2);\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::DBImpl::Lookup",
                expected: json!([
                    {"name": "rocksdb::DBImpl::Lookup", "edges": [
                        {"symbol": {"name": "rocksdb::DB::Get"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::Probe",
                expected: json!([
                    {"name": "rocksdb::Probe", "edges": [
                        {"symbol": {"name": "rocksdb::DB::Get"}},
                        {"symbol": {"name": "rocksdb::DBGetBatch::Get"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-smart-pointer-receiver.test.ts:205 — a nested class is found first from inside the class that declares it
#[tokio::test]
async fn cpp_smart_pointer_receiver_case_205_a_nested_class_is_found_first_from_inside_the_class_that_declares_it()
 {
    check_case(Case {
        name: "cpp_smart_pointer_receiver_case_205_a_nested_class_is_found_first_from_inside_the_class_that_declares_it",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator_base.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual ~IteratorBase() {}\n  virtual bool Valid() const = 0;\n  virtual void Next() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "#include \"include/rocksdb/iterator_base.h\"\nnamespace rocksdb {\nclass Iterator : public IteratorBase {\n public:\n  virtual int value() const = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/memtablerep.h",
                source: "namespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    virtual bool Valid() const = 0;\n    virtual void Next() = 0;\n  };\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/arena_wrapped_db_iter.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass ArenaWrappedDBIter : public Iterator {\n public:\n  bool Valid() const override { return true; }\n  void Next() override {}\n  int value() const override { return 0; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/db.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DB {\n public:\n  virtual int Get(int key) = 0;\n  virtual Iterator* NewIterator() = 0;\n};\nclass DBGetBatch {\n public:\n  int Get(int key) { return key; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "memtable/skiplistrep.cc",
                source: "#include <memory>\n#include \"include/rocksdb/iterator.h\"\n#include \"include/rocksdb/memtablerep.h\"\nnamespace rocksdb {\nclass SkipListRep : public MemTableRep {\n public:\n  bool Check();\n private:\n  std::unique_ptr<Iterator> iter_;\n};\nbool SkipListRep::Check() { return iter_->Valid(); }\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::SkipListRep::Check",
                expected: json!([
                    {"name": "rocksdb::SkipListRep::Check", "edges": [
                        {"symbol": {"name": "rocksdb::MemTableRep::Iterator::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-smart-pointer-receiver.test.ts:231 — a held type named through an alias, a `using namespace`, and a class local to the function
#[tokio::test]
async fn cpp_smart_pointer_receiver_case_231_a_held_type_named_through_an_alias_a_using_namespace_and_a_class_local_to_t()
 {
    check_case(Case {
        name: "cpp_smart_pointer_receiver_case_231_a_held_type_named_through_an_alias_a_using_namespace_and_a_class_local_to_t",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator_base.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual ~IteratorBase() {}\n  virtual bool Valid() const = 0;\n  virtual void Next() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "#include \"include/rocksdb/iterator_base.h\"\nnamespace rocksdb {\nclass Iterator : public IteratorBase {\n public:\n  virtual int value() const = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/memtablerep.h",
                source: "namespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    virtual bool Valid() const = 0;\n    virtual void Next() = 0;\n  };\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/arena_wrapped_db_iter.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass ArenaWrappedDBIter : public Iterator {\n public:\n  bool Valid() const override { return true; }\n  void Next() override {}\n  int value() const override { return 0; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/db.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DB {\n public:\n  virtual int Get(int key) = 0;\n  virtual Iterator* NewIterator() = 0;\n};\nclass DBGetBatch {\n public:\n  int Get(int key) { return key; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "table/internal_iterator.h",
                source: "namespace rocksdb {\ntemplate <class TValue>\nclass InternalIteratorBase {\n public:\n  virtual void SeekToFirst() = 0;\n};\nusing InternalIterator = InternalIteratorBase<int>;\nclass MergingIterator : public InternalIterator {\n public:\n  void SeekToFirst() override {}\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "table/scan.cc",
                source: "#include <memory>\n#include \"table/internal_iterator.h\"\nnamespace rocksdb {\nvoid ScanTable(InternalIterator* raw) {\n  std::unique_ptr<InternalIterator> it(raw);\n  it->SeekToFirst();\n}\nclass EventListener {\n public:\n  void OnFlushCompleted() {}\n};\nvoid Notify() {\n  class FlushListener {\n   public:\n    void OnFlushCompleted() {}\n  };\n  std::shared_ptr<FlushListener> listener;\n  listener->OnFlushCompleted();\n}\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "tools/dump.cc",
                source: "#include <memory>\n#include \"include/rocksdb/db.h\"\n#include \"include/rocksdb/memtablerep.h\"\nusing namespace rocksdb;\nint Dump(DB* db) {\n  std::unique_ptr<Iterator> it(db->NewIterator());\n  return it->Valid() ? 1 : 0;\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::ScanTable",
                expected: json!([
                    {"name": "rocksdb::ScanTable", "edges": [
                        {"symbol": {"name": "rocksdb::InternalIteratorBase::SeekToFirst"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::Notify",
                expected: json!([
                    {"name": "rocksdb::Notify", "edges": [
                        {"symbol": {"name": "rocksdb::Notify::FlushListener::OnFlushCompleted"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Dump",
                expected: json!([
                    {"name": "Dump", "edges": [
                        {"symbol": {"name": "rocksdb::DB::NewIterator"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-smart-pointer-receiver.test.ts:292 — a template parameter, a library type, and an alias of one
#[tokio::test]
async fn cpp_smart_pointer_receiver_case_292_a_template_parameter_a_library_type_and_an_alias_of_one()
 {
    check_case(Case {
        name: "cpp_smart_pointer_receiver_case_292_a_template_parameter_a_library_type_and_an_alias_of_one",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator_base.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual ~IteratorBase() {}\n  virtual bool Valid() const = 0;\n  virtual void Next() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "#include \"include/rocksdb/iterator_base.h\"\nnamespace rocksdb {\nclass Iterator : public IteratorBase {\n public:\n  virtual int value() const = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/memtablerep.h",
                source: "namespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    virtual bool Valid() const = 0;\n    virtual void Next() = 0;\n  };\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/arena_wrapped_db_iter.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass ArenaWrappedDBIter : public Iterator {\n public:\n  bool Valid() const override { return true; }\n  void Next() override {}\n  int value() const override { return 0; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/db.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DB {\n public:\n  virtual int Get(int key) = 0;\n  virtual Iterator* NewIterator() = 0;\n};\nclass DBGetBatch {\n public:\n  int Get(int key) { return key; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "table/block.h",
                source: "namespace rocksdb {\nclass Block {\n public:\n  unsigned size() const { return 0; }\n  int front() const { return 0; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db_stress_tool/stress.cc",
                source: "#include <deque>\n#include <memory>\n#include <vector>\n#include \"db/arena_wrapped_db_iter.h\"\n#include \"table/block.h\"\nnamespace rocksdb {\ntemplate <class IterType>\nvoid TestIterate(IterType* raw) {\n  std::unique_ptr<IterType> iter(raw);\n  iter->Next();\n}\nusing BlockList = std::vector<int>;\nclass BlockBasedTableIterator {\n public:\n  int First();\n private:\n  std::unique_ptr<std::deque<int>> block_handles_;\n  std::shared_ptr<BlockList> block_list_;\n};\nint BlockBasedTableIterator::First() {\n  return block_handles_->front() + static_cast<int>(block_list_->size());\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::TestIterate",
                expected: json!([
                    {"name": "rocksdb::TestIterate", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-smart-pointer-receiver.test.ts:339 — a class that has no such method, nor does any class it derives from
#[tokio::test]
async fn cpp_smart_pointer_receiver_case_339_a_class_that_has_no_such_method_nor_does_any_class_it_derives_from()
 {
    check_case(Case {
        name: "cpp_smart_pointer_receiver_case_339_a_class_that_has_no_such_method_nor_does_any_class_it_derives_from",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator_base.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual ~IteratorBase() {}\n  virtual bool Valid() const = 0;\n  virtual void Next() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "#include \"include/rocksdb/iterator_base.h\"\nnamespace rocksdb {\nclass Iterator : public IteratorBase {\n public:\n  virtual int value() const = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/memtablerep.h",
                source: "namespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    virtual bool Valid() const = 0;\n    virtual void Next() = 0;\n  };\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/arena_wrapped_db_iter.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass ArenaWrappedDBIter : public Iterator {\n public:\n  bool Valid() const override { return true; }\n  void Next() override {}\n  int value() const override { return 0; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/db.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DB {\n public:\n  virtual int Get(int key) = 0;\n  virtual Iterator* NewIterator() = 0;\n};\nclass DBGetBatch {\n public:\n  int Get(int key) { return key; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/compaction.cc",
                source: "#include <memory>\n#include \"include/rocksdb/db.h\"\n#include \"db/arena_wrapped_db_iter.h\"\nnamespace rocksdb {\nclass CompactionIterator {\n public:\n  void Release() {}\n};\nvoid Compact(DB* raw) {\n  std::unique_ptr<DB> db(raw);\n  db->Release();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Compact",
                expected: json!([
                    {"name": "rocksdb::Compact", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-smart-pointer-receiver.test.ts:367 — a type the project does not declare, such as a generated message, and a class template's specialization
#[tokio::test]
async fn cpp_smart_pointer_receiver_case_367_a_type_the_project_does_not_declare_such_as_a_generated_message_and_a_class()
 {
    check_case(Case {
        name: "cpp_smart_pointer_receiver_case_367_a_type_the_project_does_not_declare_such_as_a_generated_message_and_a_class",
        files: vec![
            SourceFile {
                path: "src/google/protobuf/message.h",
                source: "namespace google {\nnamespace protobuf {\nclass Message {\n public:\n  unsigned SpaceUsedLong() const { return 0; }\n};\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/google/protobuf/extension_set_unittest.cc",
                source: "#include <memory>\n#include \"src/google/protobuf/message.h\"\nnamespace google {\nnamespace protobuf {\nunsigned SpaceUsed() {\n  std::unique_ptr<unittest::TestAllExtensions> message;\n  return message->SpaceUsedLong();\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "util/omt.h",
                source: "#include <memory>\nnamespace toku {\ntemplate <typename omtdata_t, bool marks>\nclass omt_node {\n public:\n  void clear_stolen_bits() {}\n};\ntemplate <typename omtdata_t>\nclass omt_node<omtdata_t, true> {\n public:\n  bool get_marked() const { return false; }\n};\nbool HasMarks() {\n  std::unique_ptr<omt_node<int, true>> node;\n  return node->get_marked();\n}\n}  // namespace toku\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::SpaceUsed",
                expected: json!([
                    {"name": "google::protobuf::SpaceUsed", "edges": [
                        {"symbol": {"name": "google::protobuf::Message::SpaceUsedLong"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "toku::HasMarks",
                expected: json!([
                    {"name": "toku::HasMarks", "edges": [
                        {"symbol": {"name": "get_marked"}},
                    ]},
                ]),
                short_names: true,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-smart-pointer-receiver.test.ts:422 — `->` through a raw pointer to a smart pointer is the smart pointer's own member
#[tokio::test]
async fn cpp_smart_pointer_receiver_case_422_through_a_raw_pointer_to_a_smart_pointer_is_the_smart_pointer_s_own_member()
 {
    check_case(Case {
        name: "cpp_smart_pointer_receiver_case_422_through_a_raw_pointer_to_a_smart_pointer_is_the_smart_pointer_s_own_member",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator_base.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual ~IteratorBase() {}\n  virtual bool Valid() const = 0;\n  virtual void Next() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "#include \"include/rocksdb/iterator_base.h\"\nnamespace rocksdb {\nclass Iterator : public IteratorBase {\n public:\n  virtual int value() const = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/memtablerep.h",
                source: "namespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    virtual bool Valid() const = 0;\n    virtual void Next() = 0;\n  };\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/arena_wrapped_db_iter.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass ArenaWrappedDBIter : public Iterator {\n public:\n  bool Valid() const override { return true; }\n  void Next() override {}\n  int value() const override { return 0; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/db.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DB {\n public:\n  virtual int Get(int key) = 0;\n  virtual Iterator* NewIterator() = 0;\n};\nclass DBGetBatch {\n public:\n  int Get(int key) { return key; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "cache/cache.h",
                source: "namespace rocksdb {\nclass Cache {\n public:\n  void reset() {}\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/version_set.cc",
                source: "#include <memory>\n#include \"include/rocksdb/iterator.h\"\n#include \"cache/cache.h\"\nnamespace rocksdb {\nvoid Drop(std::unique_ptr<Iterator>* iter_cache) {\n  iter_cache->reset();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Drop",
                expected: json!([
                    {"name": "rocksdb::Drop", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-smart-pointer-receiver.test.ts:451 — a held class with a base the index does not link, which may declare the method
#[tokio::test]
async fn cpp_smart_pointer_receiver_case_451_a_held_class_with_a_base_the_index_does_not_link_which_may_declare_the_meth()
 {
    check_case(Case {
        name: "cpp_smart_pointer_receiver_case_451_a_held_class_with_a_base_the_index_does_not_link_which_may_declare_the_meth",
        files: vec![
            SourceFile {
                path: "src/google/protobuf/descriptor.cc",
                source: "#include <memory>\nnamespace google {\nnamespace protobuf {\nnamespace internal {\ntemplate <typename... T>\nclass FlatAllocatorImpl {\n public:\n  void PlanArray(int array_size) {}\n};\ntemplate <typename... T>\nFlatAllocatorImpl<T...> ApplyTypeList(int);\nclass FlatAllocator : public decltype(ApplyTypeList<char, int>(0)) {};\n}  // namespace internal\nvoid PlanAllocationSize(std::unique_ptr<internal::FlatAllocator>& alloc) {\n  alloc->PlanArray(1);\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::PlanAllocationSize",
                expected: json!([
                    {"name": "google::protobuf::PlanAllocationSize", "edges": [
                        {"symbol": {"name": "google::protobuf::internal::FlatAllocatorImpl::PlanArray"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-string-literal-declarations.test.ts:75 — reproduction: upb's generator declares `field` in the C code a raw string emits
#[tokio::test]
async fn cpp_string_literal_declarations_case_75_reproduction_upb_s_generator_declares_field_in_the_c_code_a_raw_string_emit()
 {
    check_case(Case {
        name: "cpp_string_literal_declarations_case_75_reproduction_upb_s_generator_declares_field_in_the_c_code_a_raw_string_emit",
        files: vec![
            SourceFile {
                path: "upb/reflection/def.hpp",
                source: "namespace upb {\nclass FieldDefPtr {\n public:\n  int ctype() const { return 0; }\n  const char* name() const { return \"\"; }\n};\n}  // namespace upb\n",
            },
            SourceFile {
                path: "upb/mini_table/field.h",
                source: "typedef struct upb_MiniTableField {\n  int number;\n} upb_MiniTableField;\n",
            },
            SourceFile {
                path: "upb_generator/c/generator.cc",
                source: "#include \"upb/reflection/def.hpp\"\n#include \"upb/mini_table/field.h\"\nnamespace upb {\nnamespace generator {\nvoid Emit(const char* text) {}\nvoid GenerateRepeatedSetters(upb::FieldDefPtr field) {\n  Emit(R\"cc(\n        UPB_INLINE void $0_resize(void* msg, size_t size) {\n          upb_MiniTableField field = $1;\n          // \"a quote\" and ) in the template\n          upb_Message_Resize(msg, &field, size);\n        }\n      )cc\");\n  if (field.ctype() == 1) Emit(field.name());\n}\n}  // namespace generator\n}  // namespace upb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "upb::generator::GenerateRepeatedSetters",
                expected: json!([
                    {"name": "upb::generator::GenerateRepeatedSetters", "edges": [
                        {"symbol": {"name": "upb::FieldDefPtr::ctype"}},
                        {"symbol": {"name": "upb::FieldDefPtr::name"}},
                        {"symbol": {"name": "upb::generator::Emit"}},
                    ]},
                ]),
                distinct_symbols: true,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-string-literal-declarations.test.ts:109 — an ordinary string that reads like a declaration
#[tokio::test]
async fn cpp_string_literal_declarations_case_109_an_ordinary_string_that_reads_like_a_declaration()
{
    check_case(Case {
        name: "cpp_string_literal_declarations_case_109_an_ordinary_string_that_reads_like_a_declaration",
        files: vec![
            SourceFile {
                path: "src/runner.h",
                source: "namespace demo {\nclass Runner {\n public:\n  void Run() {}\n};\nclass Stub {\n public:\n  void Run() {}\n};\nclass Printer {\n public:\n  void Print(const char* text) {}\n};\n}  // namespace demo\n",
            },
            SourceFile {
                path: "src/generator.cc",
                source: "#include \"src/runner.h\"\nnamespace demo {\nvoid Generate(Printer* printer, Runner* runner) {\n  printer->Print(\"Stub* runner = nullptr;\");\n  runner->Run();\n}\n}  // namespace demo\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "demo::Generate",
                expected: json!([
                    {"name": "demo::Generate", "edges": [
                        {"symbol": {"name": "demo::Printer::Print"}},
                        {"symbol": {"name": "demo::Runner::Run"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-string-literal-declarations.test.ts:146 — a declaration after a character literal holding a quote, or after a comment holding `R"(`
#[tokio::test]
async fn cpp_string_literal_declarations_case_146_a_declaration_after_a_character_literal_holding_a_quote_or_after_a_comment()
 {
    check_case(Case {
        name: "cpp_string_literal_declarations_case_146_a_declaration_after_a_character_literal_holding_a_quote_or_after_a_comment",
        files: vec![
            SourceFile {
                path: "src/runner.h",
                source: "namespace demo {\nclass Runner {\n public:\n  void Run() {}\n};\nclass Stub {\n public:\n  void Run() {}\n};\ninline Runner* MakeRunner() { return nullptr; }\n}  // namespace demo\n",
            },
            SourceFile {
                path: "src/quotes.cc",
                source: "#include \"src/runner.h\"\nnamespace demo {\nvoid Quote(Stub* runner) {\n  char q = '\"'; int n = 1'000; Runner* runner = MakeRunner();\n  runner->Run();\n}\nvoid Comment(Stub* runner) {\n  /* R\"( not a raw string */ Runner* runner = MakeRunner();\n  runner->Run();\n}\n}  // namespace demo\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "demo::Quote",
                expected: json!([
                    {"name": "demo::Quote", "edges": [
                        {"symbol": {"name": "demo::MakeRunner"}},
                        {"symbol": {"name": "demo::Runner::Run"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "demo::Comment",
                expected: json!([
                    {"name": "demo::Comment", "edges": [
                        {"symbol": {"name": "demo::MakeRunner"}},
                        {"symbol": {"name": "demo::Runner::Run"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-map-iterator-calls.test.ts:78 — reproduction: `it->second->Flush()` and `it->second.Flush()`
#[tokio::test]
async fn cpp_map_iterator_calls_case_78_reproduction_it_second_flush_and_it_second_flush() {
    check_case(Case {
        name: "cpp_map_iterator_calls_case_78_reproduction_it_second_flush_and_it_second_flush",
        files: vec![
            SourceFile {
                path: "src/types.h",
                source: "class Other {\n public:\n  void Flush() {}\n  void Print() const {}\n  bool ok() const { return false; }\n  int size() const { return 0; }\n};\nclass Writer {\n public:\n  void Flush() {}\n};\nclass Buffer {\n public:\n  void Flush() {}\n};\nclass Key {\n public:\n  void Print() const {}\n};\nclass Status {\n public:\n  bool ok() const { return true; }\n};\n",
            },
            SourceFile {
                path: "src/maps.cc",
                source: "#include <map>\n#include <string>\n#include \"src/types.h\"\nvoid FlushAll(std::map<std::string, Writer*>& writers) {\n  for (auto it = writers.begin(); it != writers.end(); ++it) it->second->Flush();\n}\nvoid FlushOne(std::map<int, Buffer>& buffers, int k) {\n  auto it = buffers.find(k);\n  it->second.Flush();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "FlushAll",
                expected: json!([
                    {"name": "FlushAll", "edges": [
                        {"symbol": {"name": "Writer::Flush"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "FlushOne",
                expected: json!([
                    {"name": "FlushOne", "edges": [
                        {"symbol": {"name": "Buffer::Flush"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-map-iterator-calls.test.ts:102 — a range-for's pair, a dereferenced iterator's, the key, and a local the member initializes
#[tokio::test]
async fn cpp_map_iterator_calls_case_102_a_range_for_s_pair_a_dereferenced_iterator_s_the_key_and_a_local_the_member()
 {
    check_case(Case {
        name: "cpp_map_iterator_calls_case_102_a_range_for_s_pair_a_dereferenced_iterator_s_the_key_and_a_local_the_member",
        files: vec![
            SourceFile {
                path: "src/types.h",
                source: "class Other {\n public:\n  void Flush() {}\n  void Print() const {}\n  bool ok() const { return false; }\n  int size() const { return 0; }\n};\nclass Writer {\n public:\n  void Flush() {}\n};\nclass Buffer {\n public:\n  void Flush() {}\n};\nclass Key {\n public:\n  void Print() const {}\n};\nclass Status {\n public:\n  bool ok() const { return true; }\n};\n",
            },
            SourceFile {
                path: "src/walk.cc",
                source: "#include <map>\n#include \"src/types.h\"\nvoid Each(std::map<int, Buffer>& buffers) {\n  for (auto& kv : buffers) kv.second.Flush();\n}\nvoid Deref(std::map<int, Buffer>& buffers) {\n  auto it = buffers.begin();\n  (*it).second.Flush();\n}\nvoid Keys(std::map<Key, int>& counts) {\n  auto it = counts.begin();\n  it->first.Print();\n}\nvoid Bound(std::map<int, Buffer>& buffers, int k) {\n  auto it = buffers.find(k);\n  auto& v = it->second;\n  v.Flush();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Each",
                expected: json!([
                    {"name": "Each", "edges": [
                        {"symbol": {"name": "Buffer::Flush"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Deref",
                expected: json!([
                    {"name": "Deref", "edges": [
                        {"symbol": {"name": "Buffer::Flush"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Keys",
                expected: json!([
                    {"name": "Keys", "edges": [
                        {"symbol": {"name": "Key::Print"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Bound",
                expected: json!([
                    {"name": "Bound", "edges": [
                        {"symbol": {"name": "Buffer::Flush"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-map-iterator-calls.test.ts:136 — a pair the code declares, and a member of a library type, which has no project method
#[tokio::test]
async fn cpp_map_iterator_calls_case_136_a_pair_the_code_declares_and_a_member_of_a_library_type_which_has_no_projec()
 {
    check_case(Case {
        name: "cpp_map_iterator_calls_case_136_a_pair_the_code_declares_and_a_member_of_a_library_type_which_has_no_projec",
        files: vec![
            SourceFile {
                path: "src/types.h",
                source: "class Other {\n public:\n  void Flush() {}\n  void Print() const {}\n  bool ok() const { return false; }\n  int size() const { return 0; }\n};\nclass Writer {\n public:\n  void Flush() {}\n};\nclass Buffer {\n public:\n  void Flush() {}\n};\nclass Key {\n public:\n  void Print() const {}\n};\nclass Status {\n public:\n  bool ok() const { return true; }\n};\n",
            },
            SourceFile {
                path: "src/pairs.cc",
                source: "#include <map>\n#include <string>\n#include <utility>\n#include \"src/types.h\"\nbool Check() {\n  std::pair<Status, std::string> status_and_path;\n  return status_and_path.first.ok();\n}\nint Length(std::map<int, std::string>& names) {\n  auto it = names.begin();\n  return it->second.size();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Check",
                expected: json!([
                    {"name": "Check", "edges": [
                        {"symbol": {"name": "Status::ok"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Length",
                expected: json!([
                    {"name": "Length", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-map-iterator-calls.test.ts:162 — a member another class template's parameter types goes on as before
#[tokio::test]
async fn cpp_map_iterator_calls_case_162_a_member_another_class_template_s_parameter_types_goes_on_as_before()
 {
    check_case(Case {
        name: "cpp_map_iterator_calls_case_162_a_member_another_class_template_s_parameter_types_goes_on_as_before",
        files: vec![
            SourceFile {
                path: "util/core_local.h",
                source: "#include <cstddef>\n#include <utility>\ntemplate <typename T>\nclass CoreLocalArray {\n public:\n  std::pair<T*, size_t> AccessElementAndIndex() const { return {nullptr, 0}; }\n};\n",
            },
            SourceFile {
                path: "util/cache.cc",
                source: "#include \"util/core_local.h\"\nclass ZSTDCachedData {\n public:\n  int GetUncompressData(long idx) { return 0; }\n};\nclass Rep {\n public:\n  int Get() {\n    auto p = per_core_.AccessElementAndIndex();\n    return p.first->GetUncompressData(0);\n  }\n private:\n  CoreLocalArray<ZSTDCachedData> per_core_;\n};\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Rep::Get",
                expected: json!([
                    {"name": "Rep::Get", "edges": [
                        {"symbol": {"name": "CoreLocalArray::AccessElementAndIndex"}},
                        {"symbol": {"name": "ZSTDCachedData::GetUncompressData"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-map-iterator-calls.test.ts:197 — a call on `this` is still the class's own, whatever its members are named
#[tokio::test]
async fn cpp_map_iterator_calls_case_197_a_call_on_this_is_still_the_class_s_own_whatever_its_members_are_named()
 {
    check_case(Case {
        name: "cpp_map_iterator_calls_case_197_a_call_on_this_is_still_the_class_s_own_whatever_its_members_are_named",
        files: vec![
            SourceFile {
                path: "src/types.h",
                source: "class Other {\n public:\n  void Flush() {}\n  void Print() const {}\n  bool ok() const { return false; }\n  int size() const { return 0; }\n};\nclass Writer {\n public:\n  void Flush() {}\n};\nclass Buffer {\n public:\n  void Flush() {}\n};\nclass Key {\n public:\n  void Print() const {}\n};\nclass Status {\n public:\n  bool ok() const { return true; }\n};\n",
            },
            SourceFile {
                path: "src/holder.cc",
                source: "#include \"src/types.h\"\nclass Holder {\n public:\n  void Flush() {}\n  void Run() { this->Flush(); }\n};\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Holder::Run",
                expected: json!([
                    {"name": "Holder::Run", "edges": [
                        {"symbol": {"name": "Holder::Flush"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-receiver-global-far-library.test.ts:77 — reproduction: `::upb::Arena& a` inside `namespace hpb` is the global `upb::Arena`
#[tokio::test]
async fn cpp_receiver_global_far_library_case_77_reproduction_upb_arena_a_inside_namespace_hpb_is_the_global_upb_arena()
 {
    check_case(Case {
        name: "cpp_receiver_global_far_library_case_77_reproduction_upb_arena_a_inside_namespace_hpb_is_the_global_upb_arena",
        files: vec![
            SourceFile {
                path: "upb/mem/arena.hpp",
                source: "namespace upb {\nclass Arena {\n public:\n  bool Fuse(Arena& other) { return true; }\n};\n}  // namespace upb\n",
            },
            SourceFile {
                path: "hpb/upb_arena.h",
                source: "namespace hpb {\nnamespace upb {\nclass Arena {\n public:\n  bool Fuse(Arena& other) { return false; }\n};\n}  // namespace upb\n}  // namespace hpb\n",
            },
            SourceFile {
                path: "hpb/join.cc",
                source: "#include \"upb/mem/arena.hpp\"\n#include \"hpb/upb_arena.h\"\nnamespace hpb {\nbool Join(::upb::Arena& a, ::upb::Arena& b) {\n  return a.Fuse(b);\n}\n}  // namespace hpb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "hpb::Join",
                expected: json!([
                    {"name": "hpb::Join", "edges": [
                        {"symbol": {"name": "upb::Arena::Fuse"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-receiver-global-far-library.test.ts:97 — declared over two lines, and as a range-based for loop's variable
#[tokio::test]
async fn cpp_receiver_global_far_library_case_97_declared_over_two_lines_and_as_a_range_based_for_loop_s_variable()
 {
    check_case(Case {
        name: "cpp_receiver_global_far_library_case_97_declared_over_two_lines_and_as_a_range_based_for_loop_s_variable",
        files: vec![
            SourceFile {
                path: "upb/mem/arena.hpp",
                source: "namespace upb {\nclass Arena {\n public:\n  bool Fuse(Arena& other) { return true; }\n};\n}  // namespace upb\n",
            },
            SourceFile {
                path: "hpb/upb_arena.h",
                source: "namespace hpb {\nnamespace upb {\nclass Arena {\n public:\n  bool Fuse(Arena& other) { return false; }\n};\n}  // namespace upb\n}  // namespace hpb\n",
            },
            SourceFile {
                path: "hpb/fuse.cc",
                source: "#include <vector>\n#include \"upb/mem/arena.hpp\"\n#include \"hpb/upb_arena.h\"\nnamespace hpb {\nbool Pair(::upb::Arena& b) {\n  ::upb::Arena\n      paired;\n  return paired.Fuse(b);\n}\nvoid FuseAll(std::vector<::upb::Arena>& arenas, ::upb::Arena& b) {\n  for (::upb::Arena& each : arenas) {\n    each.Fuse(b);\n  }\n}\n}  // namespace hpb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "hpb::Pair",
                expected: json!([
                    {"name": "hpb::Pair", "edges": [
                        {"symbol": {"name": "upb::Arena::Fuse"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "hpb::FuseAll",
                expected: json!([
                    {"name": "hpb::FuseAll", "edges": [
                        {"symbol": {"name": "upb::Arena::Fuse"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-receiver-global-far-library.test.ts:126 — a `::` after a template's arguments is no global scope: `Outer<T>::Iterator it` is Outer's
#[tokio::test]
async fn cpp_receiver_global_far_library_case_126_a_after_a_template_s_arguments_is_no_global_scope_outer_t_iterator_it_is_ou()
 {
    check_case(Case {
        name: "cpp_receiver_global_far_library_case_126_a_after_a_template_s_arguments_is_no_global_scope_outer_t_iterator_it_is_ou",
        files: vec![
            SourceFile {
                path: "src/outer.h",
                source: "class Iterator {\n public:\n  bool Valid() const { return false; }\n};\nnamespace ns {\ntemplate <typename T>\nclass Outer {\n public:\n  class Iterator {\n   public:\n    bool Valid() const { return true; }\n  };\n  bool Scan() {\n    Outer<T>::Iterator it;\n    return it.Valid();\n  }\n};\n}  // namespace ns\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "ns::Outer::Scan",
                expected: json!([
                    {"name": "ns::Outer::Scan", "edges": [
                        {"symbol": {"name": "ns::Outer::Iterator::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-receiver-global-far-library.test.ts:156 — is no alias of that name the calling function declares
#[tokio::test]
async fn cpp_receiver_global_far_library_case_156_is_no_alias_of_that_name_the_calling_function_declares()
 {
    check_case(Case {
        name: "cpp_receiver_global_far_library_case_156_is_no_alias_of_that_name_the_calling_function_declares",
        files: vec![
            SourceFile {
                path: "src/use.cc",
                source: "class Foo {\n public:\n  void Run() {}\n};\nclass Bar {\n public:\n  void Run() {}\n};\nvoid Use() {\n  using Foo = Bar;\n  ::Foo f;\n  f.Run();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Use",
                expected: json!([
                    {"name": "Use", "edges": [
                        {"symbol": {"name": "Foo::Run"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-receiver-global-far-library.test.ts:183 — reproduction: an `absl::flat_hash_set` is not the project's `util::flat_hash_set`
#[tokio::test]
async fn cpp_receiver_global_far_library_case_183_reproduction_an_absl_flat_hash_set_is_not_the_project_s_util_flat_hash_set()
 {
    check_case(Case {
        name: "cpp_receiver_global_far_library_case_183_reproduction_an_absl_flat_hash_set_is_not_the_project_s_util_flat_hash_set",
        files: vec![
            SourceFile {
                path: "util/hash_set.h",
                source: "namespace util {\ntemplate <class T>\nclass flat_hash_set {\n public:\n  void insert(const T& v) {}\n};\n}  // namespace util\n",
            },
            SourceFile {
                path: "util/list.h",
                source: "class List {\n public:\n  void insert(int v) {}\n};\n",
            },
            SourceFile {
                path: "db/seen.cc",
                source: "#include \"util/hash_set.h\"\n#include \"util/list.h\"\nvoid Open() {\n  absl::flat_hash_set<int> seen;\n}\nvoid Mark(int v) {\n  seen.insert(v);\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Mark",
                expected: json!([
                    {"name": "Mark", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-receiver-global-far-library.test.ts:218 — a `std::string` is not a global project class named `string`
#[tokio::test]
async fn cpp_receiver_global_far_library_case_218_a_std_string_is_not_a_global_project_class_named_string()
 {
    check_case(Case {
        name: "cpp_receiver_global_far_library_case_218_a_std_string_is_not_a_global_project_class_named_string",
        files: vec![
            SourceFile {
                path: "src/string.h",
                source: "class string {\n public:\n  void clear() {}\n};\n",
            },
            SourceFile {
                path: "src/buffer.h",
                source: "class Buffer {\n public:\n  void clear() {}\n};\n",
            },
            SourceFile {
                path: "src/text.cc",
                source: "#include <string>\n#include \"src/string.h\"\n#include \"src/buffer.h\"\nvoid Load() {\n  std::string text;\n}\nvoid Reset() {\n  text.clear();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Reset",
                expected: json!([
                    {"name": "Reset", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-receiver-global-far-library.test.ts:251 — the project's own lowercase type is still its own
#[tokio::test]
async fn cpp_receiver_global_far_library_case_251_the_project_s_own_lowercase_type_is_still_its_own()
 {
    check_case(Case {
        name: "cpp_receiver_global_far_library_case_251_the_project_s_own_lowercase_type_is_still_its_own",
        files: vec![
            SourceFile {
                path: "util/hash_set.h",
                source: "namespace util {\ntemplate <class T>\nclass flat_hash_set {\n public:\n  void insert(const T& v) {}\n};\n}  // namespace util\n",
            },
            SourceFile {
                path: "db/keep.cc",
                source: "#include \"util/hash_set.h\"\nvoid Open() {\n  util::flat_hash_set<int> mine;\n}\nvoid Keep(int v) {\n  mine.insert(v);\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Keep",
                expected: json!([
                    {"name": "Keep", "edges": [
                        {"symbol": {"name": "util::flat_hash_set::insert"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// go-bare-call-no-method.test.ts:48
#[tokio::test]
async fn go_parameter_calls_do_not_bind_to_same_package_methods() {
    check_case(Case {
        name: "go_parameter_calls_do_not_bind_to_same_package_methods",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: "module example.com/app\n\ngo 1.22\n",
            },
            SourceFile {
                path: "loop/loop.go",
                source: "package loop\n\ntype Loop struct{}\n\nfunc (l *Loop) relogin() error { return nil }\n",
            },
            SourceFile {
                path: "loop/sched.go",
                source: "package loop\n\nfunc runScheduler(relogin func() error) error {\n\tdefer relogin()\n\tgo relogin()\n\treturn relogin()\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "runScheduler",
                expected: json!([
                    {"name": "runScheduler", "edges": [
                        {"symbol": {"name": "Loop::relogin", "filePath": "loop/loop.go"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// go-bare-call-no-method.test.ts:76
#[tokio::test]
async fn go_parameter_calls_do_not_bind_to_unimported_package_methods() {
    check_case(Case {
        name: "go_parameter_calls_do_not_bind_to_unimported_package_methods",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: "module example.com/app\n\ngo 1.22\n",
            },
            SourceFile {
                path: "impulse/loop.go",
                source: "package impulse\n\ntype Loop struct{}\n\nfunc (l *Loop) Relogin() error { return nil }\n",
            },
            SourceFile {
                path: "synapse/sched.go",
                source: "package synapse\n\nfunc RunScheduler(Relogin func() error) error {\n\tif err := Relogin(); err != nil {\n\t\treturn err\n\t}\n\treturn nil\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "RunScheduler",
                expected: json!([
                    {"name": "RunScheduler", "edges": [
                        {"symbol": {"name": "Loop::Relogin", "filePath": "impulse/loop.go"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// go-bare-call-no-method.test.ts:105
#[tokio::test]
async fn go_bare_calls_keep_the_same_package_function() {
    check_case(Case {
        name: "go_bare_calls_keep_the_same_package_function",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: "module example.com/app\n\ngo 1.22\n",
            },
            SourceFile {
                path: "loop/loop.go",
                source: "package loop\n\ntype Loop struct{}\n\nfunc (l *Loop) flush() {}\n\nfunc flush() {}\n\nfunc run() {\n\tflush()\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"name": "run", "edges": [
                        {"symbol": {"name": "flush", "filePath": "loop/loop.go"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"name": "run", "edges": [
                        {"symbol": {"name": "Loop::flush", "filePath": "loop/loop.go"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// go-bare-call-no-method.test.ts:130
#[tokio::test]
async fn go_receiver_calls_keep_the_method() {
    check_case(Case {
        name: "go_receiver_calls_keep_the_method",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: "module example.com/app\n\ngo 1.22\n",
            },
            SourceFile {
                path: "loop/loop.go",
                source: "package loop\n\ntype Loop struct{}\n\nfunc (l *Loop) relogin() error { return nil }\n\nfunc (l *Loop) run() error {\n\treturn l.relogin()\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Loop::run",
                expected: json!([
                    {"name": "Loop::run", "edges": [
                        {"symbol": {"name": "Loop::relogin", "filePath": "loop/loop.go"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// go-bare-call-no-method.test.ts:152
#[tokio::test]
async fn go_package_factory_chain_keeps_the_method() {
    check_case(Case {
        name: "go_package_factory_chain_keeps_the_method",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: "module example.com/app\n\ngo 1.22\n",
            },
            SourceFile {
                path: "service/table.go",
                source: "package service\n\ntype Table struct{}\n\nfunc (t *Table) Reload() error { return nil }\n\nfunc SysTable() *Table { return &Table{} }\n",
            },
            SourceFile {
                path: "controller/ctl.go",
                source: "package controller\n\nimport \"example.com/app/service\"\n\nfunc Handle() error {\n\treturn service.SysTable().Reload()\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Handle",
                expected: json!([
                    {"name": "Handle", "edges": [
                        {"symbol": {"name": "Table::Reload", "filePath": "service/table.go"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// python-self-call-not-import.test.ts:44
#[tokio::test]
async fn python_self_call_does_not_bind_to_same_named_import() {
    check_case(Case {
        name: "python_self_call_does_not_bind_to_same_named_import",
        files: vec![
            SourceFile {
                path: "pkg/__init__.py",
                source: "",
            },
            SourceFile {
                path: "pkg/httpkit.py",
                source: "def get_ip(request):\n    return \"1.2.3.4\"\n",
            },
            SourceFile {
                path: "pkg/adapter.py",
                source: "from pkg.httpkit import get_ip\n\n\nclass Adapter:\n    def get_ip(self, request):\n        return get_ip(request)\n\n    def notify(self, request):\n        return self.get_ip(request)\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Adapter::get_ip",
                expected: json!([
                    {"name": "Adapter::get_ip", "edges": [
                        {"symbol": {"name": "get_ip", "filePath": "pkg/httpkit.py"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Adapter::notify",
                expected: json!([
                    {"name": "Adapter::notify", "edges": [
                        {"symbol": {"name": "Adapter::get_ip", "filePath": "pkg/adapter.py"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// python-self-call-not-import.test.ts:69
#[tokio::test]
async fn python_inherited_call_does_not_bind_to_same_named_import() {
    check_case(Case {
        name: "python_inherited_call_does_not_bind_to_same_named_import",
        files: vec![
            SourceFile {
                path: "pkg/__init__.py",
                source: "",
            },
            SourceFile {
                path: "pkg/httpkit.py",
                source: "def get_ip(request):\n    return \"1.2.3.4\"\n",
            },
            SourceFile {
                path: "pkg/base.py",
                source: "class Base:\n    def get_ip(self, request):\n        return \"base\"\n",
            },
            SourceFile {
                path: "pkg/adapter.py",
                source: "from pkg.base import Base\nfrom pkg.httpkit import get_ip\n\n\nclass Adapter(Base):\n    def notify(self, request):\n        return self.get_ip(request)\n\n    def raw(self, request):\n        return get_ip(request)\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Adapter::notify",
                expected: json!([
                    {"name": "Adapter::notify", "edges": [
                        {"symbol": {"name": "Base::get_ip", "filePath": "pkg/base.py"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Adapter::raw",
                expected: json!([
                    {"name": "Adapter::raw", "edges": [
                        {"symbol": {"name": "get_ip", "filePath": "pkg/httpkit.py"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// c-static-call-rescue.test.ts:99
#[tokio::test]
async fn c_included_header_beats_neighbouring_static_function() {
    check_case(Case {
        name: "c_included_header_beats_neighbouring_static_function",
        files: vec![
            SourceFile {
                path: "test/include/test/arena_util.h",
                source: "static inline unsigned\ndo_arena_create(long dirty_decay_ms, long muzzy_decay_ms) {\n  return 0;\n}\n",
            },
            SourceFile {
                path: "test/unit/arena_reset.c",
                source: "static unsigned\ndo_arena_create(void *hooks) {\n  return 1;\n}\nvoid test_reset(void) { do_arena_create(0); }\n",
            },
            SourceFile {
                path: "test/unit/arena_decay.c",
                source: "#include \"test/include/test/arena_util.h\"\nvoid test_decay(void) { unsigned arena = do_arena_create(0, 0); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "test_decay",
                expected: json!([
                    {"name": "test_decay", "edges": [
                        {"symbol": {"filePath": "test/include/test/arena_util.h", "startLine": 1}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "test_reset",
                expected: json!([
                    {"name": "test_reset", "edges": [
                        {"symbol": {"filePath": "test/unit/arena_reset.c", "startLine": 1}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// c-static-call-rescue.test.ts:105
#[tokio::test]
async fn c_declaration_reaches_external_definition_not_foreign_static() {
    check_case(Case {
        name: "c_declaration_reaches_external_definition_not_foreign_static",
        files: vec![
            SourceFile {
                path: "lib/helper.h",
                source: "int helper(int x);\n",
            },
            SourceFile {
                path: "lib/helper.c",
                source: "#include \"lib/helper.h\"\nint helper(int x) { return x; }\n",
            },
            SourceFile {
                path: "app/other.c",
                source: "static int helper(int x) { return -x; }\nint other(void) { return helper(1); }\n",
            },
            SourceFile {
                path: "app/main.c",
                source: "#include \"lib/helper.h\"\nint run(void) { return helper(2); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"name": "run", "edges": [
                        {"symbol": {"filePath": "lib/helper.c", "startLine": 2}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "other",
                expected: json!([
                    {"name": "other", "edges": [
                        {"symbol": {"filePath": "app/other.c", "startLine": 1}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// c-static-call-rescue.test.ts:119
#[tokio::test]
async fn c_included_definition_beats_closer_external_definition() {
    check_case(Case {
        name: "c_included_definition_beats_closer_external_definition",
        files: vec![
            SourceFile {
                path: "include/timer.h",
                source: "static inline long now_ns(void) { return 0; }\n",
            },
            SourceFile {
                path: "bench/unit/clock.c",
                source: "static long now_ns(void) { return 1; }\n",
            },
            SourceFile {
                path: "bench/other/fake_clock.c",
                source: "long now_ns(void) { return 2; }\n",
            },
            SourceFile {
                path: "bench/unit/latency.c",
                source: "#include \"include/timer.h\"\nlong elapsed(void) { return now_ns(); }\n",
            },
            SourceFile {
                path: "include/ticks.h",
                source: "static inline long ticks(void) { return 0; }\n",
            },
            SourceFile {
                path: "bench/unit/ticks.c",
                source: "long ticks(void) { return 1; }\n",
            },
            SourceFile {
                path: "bench/unit/loop.c",
                source: "#include \"include/ticks.h\"\nlong spin(void) { return ticks(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "elapsed",
                expected: json!([
                    {"name": "elapsed", "edges": [
                        {"symbol": {"filePath": "include/timer.h", "startLine": 1}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "spin",
                expected: json!([
                    {"name": "spin", "edges": [
                        {"symbol": {"filePath": "include/ticks.h", "startLine": 1}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    })
    .await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// java-declared-receiver.test.ts:121
// java-declared-receiver.test.ts:125
// java-declared-receiver.test.ts:129
#[tokio::test]
async fn java_generic_parameter_uses_declared_receiver_type() {
    check_case(Case {
        name: "java_generic_parameter_uses_declared_receiver_type",
        files: vec![
            SourceFile {
                path: "src/main/java/app/Pair.java",
                source: "package app;\n\npublic abstract class Pair<L, R> {\n    public abstract L getKey();\n}\n",
            },
            SourceFile {
                path: "src/main/java/app/MutablePair.java",
                source: "package app;\n\npublic class MutablePair<L, R> extends Pair<L, R> {\n    private L left;\n\n    public L getKey() { return left; }\n\n    /**\n     * Copies the key of each element into this pair.\n     */\n    public void setValue(final Pair<L, R> pair) {\n        left = pair.getKey();\n    }\n}\n",
            },
            SourceFile {
                path: "src/main/java/app/DateParser.java",
                source: "package app;\n\npublic interface DateParser {\n    Object parse(String source);\n}\n",
            },
            SourceFile {
                path: "src/main/java/app/Element.java",
                source: "package app;\n\npublic class Element {\n    public void val(String value) { }\n}\n",
            },
            SourceFile {
                path: "src/main/java/app/Elements.java",
                source: "package app;\n\nimport java.util.ArrayList;\n\npublic class Elements extends ArrayList<Element> {\n    public Elements val(String value) {\n        for (Element element : this)\n            element.val(value);\n        return this;\n    }\n}\n",
            },
            SourceFile {
                path: "src/main/java/app/Phase.java",
                source: "package app;\n\npublic enum Phase {\n    CONTROLLERS;\n\n    public int getPhase() { return 0; }\n}\n",
            },
            SourceFile {
                path: "src/main/java/app/ExceptionContext.java",
                source: "package app;\n\npublic interface ExceptionContext {\n    int getContextEntries();\n}\n",
            },
            SourceFile {
                path: "src/test/java/app/DateUtilsTest.java",
                source: "package app;\n\nimport java.text.DateFormat;\n\npublic abstract class DateUtilsTest<T extends ExceptionContext> {\n    private DateFormat dateTimeParser;\n    protected T exceptionContext;\n\n    void rounds() throws Exception {\n        dateTimeParser.parse(\"January 1, 2008\");\n        exceptionContext.getContextEntries();\n        Phase.CONTROLLERS.getPhase();\n    }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "setValue",
                expected: json!([
                    {"filePath": "src/main/java/app/MutablePair.java", "edges": [
                        {"symbol": {"name": "getKey", "filePath": "src/main/java/app/Pair.java"}},
                    ]},
                ]),
                file: Some("src/main/java/app/MutablePair.java"),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "val",
                expected: json!([
                    {"filePath": "src/main/java/app/Elements.java", "edges": [
                        {"symbol": {"name": "val", "filePath": "src/main/java/app/Element.java"}},
                    ]},
                ]),
                file: Some("src/main/java/app/Elements.java"),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rounds",
                expected: json!([
                    {"filePath": "src/test/java/app/DateUtilsTest.java", "edges": [
                        {"symbol": {"name": "getContextEntries", "filePath": "src/main/java/app/ExceptionContext.java"}},
                        {"symbol": {"name": "getPhase", "filePath": "src/main/java/app/Phase.java"}},
                    ]},
                ]),
                file: Some("src/test/java/app/DateUtilsTest.java"),
                short_names: true,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:44
#[tokio::test]
async fn alias_exported_binding_reaches_function() {
    check_case(Case {
        name: "alias_exported_binding_reaches_function",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "export function realImpl(): number { return 1; }\nexport const aliasName = realImpl;\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { aliasName } from './impl';\nexport function consumerFn(): number { return aliasName(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::realImpl",
                expected: json!([
                    {"name": "realImpl", "edges": [
                        {"symbol": {"name": "consumerFn"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:52
#[tokio::test]
async fn alias_renamed_export_reaches_function() {
    check_case(Case {
        name: "alias_renamed_export_reaches_function",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function realImpl(): number { return 1; }\nexport { realImpl as aliasName };\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { aliasName } from './impl';\nexport function consumerFn(): number { return aliasName(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::realImpl",
                expected: json!([
                    {"name": "realImpl", "edges": [
                        {"symbol": {"name": "consumerFn"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:62
#[tokio::test]
async fn alias_object_property_reaches_function() {
    check_case(Case {
        name: "alias_object_property_reaches_function",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "export function realImpl(): number { return 1; }\nexport const api = { run: realImpl };\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function consumerFn(): number { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::realImpl",
                expected: json!([
                    {"name": "realImpl", "edges": [
                        {"symbol": {"name": "consumerFn"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row sibling_literals)
#[tokio::test]
async fn alias_ts_sibling_literals() {
    check_case(Case {
        name: "alias_ts_sibling_literals",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nconst first = { wrong }; export const api = { run: right };\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row sibling_literals)
#[tokio::test]
async fn alias_js_sibling_literals() {
    check_case(Case {
        name: "alias_js_sibling_literals",
        files: vec![
            SourceFile {
                path: "impl.js",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nconst first = { wrong }; export const api = { run: right };\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.js",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row unicode_before_literal)
#[tokio::test]
async fn alias_ts_unicode_before_literal() {
    check_case(Case {
        name: "alias_ts_unicode_before_literal",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nconst label = 'é🙂'; export const api = { run: right };\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row unicode_before_literal)
#[tokio::test]
async fn alias_js_unicode_before_literal() {
    check_case(Case {
        name: "alias_js_unicode_before_literal",
        files: vec![
            SourceFile {
                path: "impl.js",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nconst label = 'é🙂'; export const api = { run: right };\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.js",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row absent_sibling_member)
#[tokio::test]
async fn alias_ts_absent_sibling_member() {
    check_case(Case {
        name: "alias_ts_absent_sibling_member",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nconst first = { wrong }; export const api = { right };\nexport function sameCaller() { return api.wrong(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.wrong(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row absent_sibling_member)
#[tokio::test]
async fn alias_js_absent_sibling_member() {
    check_case(Case {
        name: "alias_js_absent_sibling_member",
        files: vec![
            SourceFile {
                path: "impl.js",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nconst first = { wrong }; export const api = { right };\nexport function sameCaller() { return api.wrong(); }\n",
            },
            SourceFile {
                path: "consumer.js",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.wrong(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row duplicate_key)
#[tokio::test]
async fn alias_ts_duplicate_key() {
    check_case(Case {
        name: "alias_ts_duplicate_key",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { run: wrong, run: right };\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row duplicate_key)
#[tokio::test]
async fn alias_js_duplicate_key() {
    check_case(Case {
        name: "alias_js_duplicate_key",
        files: vec![
            SourceFile {
                path: "impl.js",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { run: wrong, run: right };\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.js",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row non_callable_overwrite)
#[tokio::test]
async fn alias_ts_non_callable_overwrite() {
    check_case(Case {
        name: "alias_ts_non_callable_overwrite",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { wrong, wrong: 0 };\nexport function sameCaller() { return api.wrong(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.wrong(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row non_callable_overwrite)
#[tokio::test]
async fn alias_js_non_callable_overwrite() {
    check_case(Case {
        name: "alias_js_non_callable_overwrite",
        files: vec![
            SourceFile {
                path: "impl.js",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { wrong, wrong: 0 };\nexport function sameCaller() { return api.wrong(); }\n",
            },
            SourceFile {
                path: "consumer.js",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.wrong(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row nested_member)
#[tokio::test]
async fn alias_ts_nested_member() {
    check_case(Case {
        name: "alias_ts_nested_member",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { box: { wrong }, method() { return { wrong }; } };\nexport function sameCaller() { return api.wrong(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.wrong(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row nested_member)
#[tokio::test]
async fn alias_js_nested_member() {
    check_case(Case {
        name: "alias_js_nested_member",
        files: vec![
            SourceFile {
                path: "impl.js",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { box: { wrong }, method() { return { wrong }; } };\nexport function sameCaller() { return api.wrong(); }\n",
            },
            SourceFile {
                path: "consumer.js",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.wrong(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row unknown_spread)
#[tokio::test]
async fn alias_ts_unknown_spread() {
    check_case(Case {
        name: "alias_ts_unknown_spread",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { wrong, ...unknown };\nexport function sameCaller() { return api.wrong(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.wrong(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row unknown_spread)
#[tokio::test]
async fn alias_js_unknown_spread() {
    check_case(Case {
        name: "alias_js_unknown_spread",
        files: vec![
            SourceFile {
                path: "impl.js",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { wrong, ...unknown };\nexport function sameCaller() { return api.wrong(); }\n",
            },
            SourceFile {
                path: "consumer.js",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.wrong(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row explicit_after_spread)
#[tokio::test]
async fn alias_ts_explicit_after_spread() {
    check_case(Case {
        name: "alias_ts_explicit_after_spread",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { ...unknown, run: right };\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row explicit_after_spread)
#[tokio::test]
async fn alias_js_explicit_after_spread() {
    check_case(Case {
        name: "alias_js_explicit_after_spread",
        files: vec![
            SourceFile {
                path: "impl.js",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { ...unknown, run: right };\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.js",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row quoted_overwrite)
#[tokio::test]
async fn alias_ts_quoted_overwrite() {
    check_case(Case {
        name: "alias_ts_quoted_overwrite",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { wrong, 'wrong': 0 };\nexport function sameCaller() { return api.wrong(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.wrong(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row quoted_overwrite)
#[tokio::test]
async fn alias_js_quoted_overwrite() {
    check_case(Case {
        name: "alias_js_quoted_overwrite",
        files: vec![
            SourceFile {
                path: "impl.js",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { wrong, 'wrong': 0 };\nexport function sameCaller() { return api.wrong(); }\n",
            },
            SourceFile {
                path: "consumer.js",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.wrong(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row computed_overwrite)
#[tokio::test]
async fn alias_ts_computed_overwrite() {
    check_case(Case {
        name: "alias_ts_computed_overwrite",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { wrong, [unknown]: 0 };\nexport function sameCaller() { return api.wrong(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.wrong(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row computed_overwrite)
#[tokio::test]
async fn alias_js_computed_overwrite() {
    check_case(Case {
        name: "alias_js_computed_overwrite",
        files: vec![
            SourceFile {
                path: "impl.js",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { wrong, [unknown]: 0 };\nexport function sameCaller() { return api.wrong(); }\n",
            },
            SourceFile {
                path: "consumer.js",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.wrong(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row frozen_literal)
#[tokio::test]
async fn alias_ts_frozen_literal() {
    check_case(Case {
        name: "alias_ts_frozen_literal",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = Object.freeze({ run: right });\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row frozen_literal)
#[tokio::test]
async fn alias_js_frozen_literal() {
    check_case(Case {
        name: "alias_js_frozen_literal",
        files: vec![
            SourceFile {
                path: "impl.js",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = Object.freeze({ run: right });\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.js",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row parenthesized_literal)
#[tokio::test]
async fn alias_ts_parenthesized_literal() {
    check_case(Case {
        name: "alias_ts_parenthesized_literal",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = ({ run: right });\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row parenthesized_literal)
#[tokio::test]
async fn alias_js_parenthesized_literal() {
    check_case(Case {
        name: "alias_js_parenthesized_literal",
        files: vec![
            SourceFile {
                path: "impl.js",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = ({ run: right });\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.js",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row inline_overwrite)
#[tokio::test]
async fn alias_ts_inline_overwrite() {
    check_case(Case {
        name: "alias_ts_inline_overwrite",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { run() { return 0; }, run: right };\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "run",
                expected: json!([
                    {"name": "run", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                any_definition: true,
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row inline_overwrite)
#[tokio::test]
async fn alias_js_inline_overwrite() {
    check_case(Case {
        name: "alias_js_inline_overwrite",
        files: vec![
            SourceFile {
                path: "impl.js",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { run() { return 0; }, run: right };\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.js",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "run",
                expected: json!([
                    {"name": "run", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                any_definition: true,
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row nested_inline)
#[tokio::test]
async fn alias_ts_nested_inline() {
    check_case(Case {
        name: "alias_ts_nested_inline",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { box: { wrong() { return 0; } } };\nexport function sameCaller() { return api.wrong(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.wrong(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:71 (original it.each row nested_inline)
#[tokio::test]
async fn alias_js_nested_inline() {
    check_case(Case {
        name: "alias_js_nested_inline",
        files: vec![
            SourceFile {
                path: "impl.js",
                source: "function wrong() { return 1; }\nfunction right() { return 2; }\nexport const api = { box: { wrong() { return 0; } } };\nexport function sameCaller() { return api.wrong(); }\n",
            },
            SourceFile {
                path: "consumer.js",
                source: "import { api } from './impl';\nexport function crossCaller() { return api.wrong(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::wrong",
                expected: json!([
                    {"name": "wrong", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "::right",
                expected: json!([
                    {"name": "right", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:110
#[tokio::test]
async fn alias_parameter_and_value_shadows_do_not_reach_outer_function() {
    check_case(Case {
        name: "alias_parameter_and_value_shadows_do_not_reach_outer_function",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function target() { return 0; }\nexport function parameter(target: () => number) {\n  const api = { target };\n  return api.target();\n}\nexport function value() {\n  const target = 0;\n  const api = { target };\n  return api.target();\n}\nexport function later() {\n  const api = { target };\n  const target = 0;\n  return api.target();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::target",
                expected: json!([
                    {"name": "target", "edges": [
                        {"symbol": {"name": "parameter"}},
                        {"symbol": {"name": "value"}},
                        {"symbol": {"name": "later"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:135
#[tokio::test]
async fn alias_renamed_import_ignores_unrelated_nested_binding() {
    check_case(Case {
        name: "alias_renamed_import_ignores_unrelated_nested_binding",
        files: vec![
            SourceFile {
                path: "target.ts",
                source: "export function actual() { return 1; }",
            },
            SourceFile {
                path: "impl.ts",
                source: "import { actual as renamed } from './target';\nfunction unrelated() { function renamed() { return 0; } return renamed(); }\nexport const api = { run: renamed };\nexport function sameCaller() { return api.run(); }\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { api as facade } from './impl';\nexport function crossCaller() { return facade.run(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::actual",
                expected: json!([
                    {"name": "actual", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "unrelated::renamed",
                expected: json!([
                    {"name": "unrelated::renamed", "edges": [
                        {"symbol": {"name": "sameCaller"}},
                        {"symbol": {"name": "crossCaller"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:152
#[tokio::test]
async fn alias_same_file_alias_reaches_function() {
    check_case(Case {
        name: "alias_same_file_alias_reaches_function",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "function realImpl(): number { return 1; }\nconst localAlias = realImpl;\nexport function consumerFn(): number { return localAlias(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::realImpl",
                expected: json!([
                    {"name": "realImpl", "edges": [
                        {"symbol": {"name": "consumerFn"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:162
#[tokio::test]
async fn alias_real_wrapper_is_not_treated_as_alias() {
    check_case(Case {
        name: "alias_real_wrapper_is_not_treated_as_alias",
        files: vec![
            SourceFile {
                path: "impl.ts",
                source: "export function realImpl(): number { return 1; }\nexport const wrapper = (): number => realImpl();\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { wrapper } from './impl';\nexport function consumerFn(): number { return wrapper(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "::realImpl",
                expected: json!([
                    {"name": "realImpl", "edges": [
                        {"symbol": {"name": "consumerFn"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// alias-binding-resolution.test.ts:173
#[tokio::test]
async fn alias_ambiguous_alias_does_not_guess_a_function() {
    check_case(Case {
        name: "alias_ambiguous_alias_does_not_guess_a_function",
        files: vec![
            SourceFile {
                path: "one.ts",
                source: "export function shared(): number { return 1; }\n",
            },
            SourceFile {
                path: "two.ts",
                source: "export function shared(): number { return 2; }\n",
            },
            SourceFile {
                path: "alias.ts",
                source: "import { shared } from './one';\nexport const aliasName = shared;\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { aliasName } from './alias';\nexport function consumerFn(): number { return aliasName(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "shared",
                expected: json!([
                    {"name": "shared", "filePath": "one.ts", "edges": [
                        {"symbol": {"name": "consumerFn"}},
                    ]},
                    {"name": "shared", "filePath": "two.ts", "edges": [
                        {"symbol": {"name": "consumerFn"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-free-function-overloads.test.ts:206
// cpp-free-function-overloads.test.ts:215
// cpp-free-function-overloads.test.ts:222
// cpp-free-function-overloads.test.ts:227
// cpp-free-function-overloads.test.ts:234
// cpp-free-function-overloads.test.ts:238
// cpp-free-function-overloads.test.ts:242
// cpp-free-function-overloads.test.ts:246
// cpp-free-function-overloads.test.ts:253
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn cpp_argument_types_select_the_overload() {
    check_case(Case {
        name: "cpp_argument_types_select_the_overload",
        files: vec![
            SourceFile {
                path: "src/varint.cc",
                source: "namespace pb {\nconst char* ParseWithLimit(int rtlimit, const char* data, int32_t& res) { return data; }\nconst char* ParseWithLimit(int rtlimit, const char* data, int64_t& res) { return data; }\nvoid Parse32(const char* data) { int32_t result; ParseWithLimit(1, data, result); }\nint Scale(int x, int y) { return x * y; }\nint Scale(int x) { return x; }\nint Twice(int v) { return Scale(v, 2); }\nvoid Put(const char* text) {}\nvoid Put(int value) {}\nvoid PutText() { Put(\"text\"); }\ntemplate <typename T> void Store(T value) {}\nvoid Store(int value) {}\nvoid StoreName(const std::string& name) { Store(name); }\nvoid StoreCount() { Store(1); }\n}  // namespace pb\n",
            },
            SourceFile {
                path: "src/render.cc",
                source: "namespace pb {\nstruct Circle {}; struct Square {}; struct Shape {};\nvoid Render(const Circle& c) {}\nvoid Render(const Square& s) {}\nvoid Draw(const Shape& shape) { Render(shape); }\n}  // namespace pb\n",
            },
            SourceFile {
                path: "src/print.cc",
                source: "namespace a {\nstruct Message { Message(const char* text) {} };\nvoid Print(const Message& m) {}\nvoid Hello() { Print(\"hi\"); }\n}  // namespace a\nnamespace b {\nvoid Print(const char* text) {}\n}  // namespace b\n",
            },
            SourceFile {
                path: "src/codec.h",
                source: "namespace codec {\ninline void Encode(const char* text, char* out) {}\ninline void Encode(long value, char* out) {}\ninline void EncodeHere(char* out) { codec::Encode(7L, out); }\n}  // namespace codec\n",
            },
            SourceFile {
                path: "src/codec.cc",
                source: "#include \"src/codec.h\"\nvoid EncodeThere(char* out) { codec::Encode(7L, out); }\nvoid EncodeGlobal(char* out) { ::codec::Encode(7L, out); }\n",
            },
            SourceFile {
                path: "src/serialize.h",
                source: "namespace ns {\ntemplate <typename T> void Serialize(T val, char* dest);\ntemplate <> inline void Serialize<char>(char t, char* dest) {}\ntemplate <> inline void Serialize<long>(long t, char* dest) {}\n}  // namespace ns\n",
            },
            SourceFile {
                path: "src/serialize.cc",
                source: "#include \"src/serialize.h\"\nvoid WriteLong(char* dest) { ns::Serialize(1L, dest); }\n",
            },
            SourceFile {
                path: "src/log.h",
                source: "namespace lg {\nstruct Status { Status(int code) {} };\nvoid Log(const Status& status);\nvoid Warn(const Status& status);\n}  // namespace lg\n",
            },
            SourceFile {
                path: "src/log.cc",
                source: "#include \"src/log.h\"\nnamespace lg {\nvoid Log(const Status& status) {}\nvoid Warn(const Status& status) {}\n}  // namespace lg\n",
            },
            SourceFile {
                path: "src/log_private.cc",
                source: "namespace lg {\nstatic void Log(int level) {}\n}  // namespace lg\n",
            },
            SourceFile {
                path: "src/a_private.cc",
                source: "namespace lg {\nstatic void Warn(int level) {}\n}  // namespace lg\n",
            },
            SourceFile {
                path: "src/log_use.cc",
                source: "#include \"src/log.h\"\nvoid LogCode() { lg::Log(3); }\nvoid WarnCode() { lg::Warn(3); }\n",
            },
            SourceFile {
                path: "src/wire.h",
                source: "namespace wire {\nstruct Wire {};\nWire Fixed64(uint64_t x);\ntemplate <typename T> Wire Fixed64(T x) { return Fixed64(static_cast<uint64_t>(x)); }\n}  // namespace wire\n",
            },
            SourceFile {
                path: "src/wire.cc",
                source: "#include \"src/wire.h\"\nnamespace wire {\nWire Fixed64(uint64_t x) { return Wire(); }\n}  // namespace wire\n",
            },
            SourceFile {
                path: "src/wire_use.cc",
                source: "#include \"src/wire.h\"\nstd::string u64(uint64_t u64) {\n  wire::Fixed64(u64);\n  return \"\";\n}\nstruct Counter { Counter(int n) {} int value() const { return 0; } };\nstruct Gauge { int value() const { return 1; } };\nint count(int n) {\n  Counter count(n);\n  return count.value();\n}\n",
            },
            SourceFile {
                path: "src/stat.h",
                source: "namespace sys {\nstruct stat { long size; };\ninline int stat(const char* path, struct stat* out) { return 0; }\n}  // namespace sys\n",
            },
            SourceFile {
                path: "src/stat.cc",
                source: "#include \"src/stat.h\"\nlong FileSize(const char* path) { sys::stat st; sys::stat(path, &st); return st.size; }\nstruct Sized : sys::stat {};\n",
            },
            SourceFile {
                path: "src/rust/upb/lib.rs",
                source: "mod sys;\nmod wire;\n",
            },
            SourceFile {
                path: "src/rust/upb/sys/mod.rs",
                source: "pub mod wire;\n",
            },
            SourceFile {
                path: "src/rust/upb/sys/wire/mod.rs",
                source: "pub mod wire;\n",
            },
            SourceFile {
                path: "src/rust/upb/sys/wire/wire.rs",
                source: "extern \"C\" {\n    pub fn upb_encode(msg: *const u8, len: usize) -> i32;\n}\n",
            },
            SourceFile {
                path: "src/rust/upb/wire.rs",
                source: "use super::sys::wire::wire::{upb_encode};\npub fn encode(msg: *const u8) -> i32 {\n    unsafe { upb_encode(msg, 0) }\n}\n",
            },
            SourceFile {
                path: "src/upb/wire/encode.c",
                source: "int upb_encode(const char* msg, unsigned long len) { return 0; }\n",
            },
            SourceFile {
                path: "src/fmtlike.h",
                source: "#define MY_BEGIN_NAMESPACE namespace mylib {\n#define MY_END_NAMESPACE }\nMY_BEGIN_NAMESPACE\ninline void write(char* out, const char* text) {}\ninline void write(char* out, int value) {}\nMY_END_NAMESPACE\n",
            },
            SourceFile {
                path: "src/fmtlike.cc",
                source: "#include \"src/fmtlike.h\"\nvoid WriteCount(char* out, int count) { mylib::write(out, count); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "pb::Parse32",
                expected: json!([
                    {"name": "pb::Parse32", "edges": [
                        {"symbol": {"name": "pb::ParseWithLimit", "filePath": "src/varint.cc", "startLine": 2}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::StoreName",
                expected: json!([
                    {"name": "pb::StoreName", "edges": [
                        {"symbol": {"name": "pb::Store", "filePath": "src/varint.cc", "startLine": 11}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::StoreCount",
                expected: json!([
                    {"name": "pb::StoreCount", "edges": [
                        {"symbol": {"name": "pb::Store", "filePath": "src/varint.cc", "startLine": 12}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "u64",
                expected: json!([
                    {"name": "u64", "edges": [
                        {"symbol": {"name": "wire::Fixed64", "filePath": "src/wire.cc", "startLine": 3}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "FileSize",
                expected: json!([
                    {"name": "FileSize", "edges": [
                        {"symbol": {"name": "sys::stat", "filePath": "src/stat.h", "startLine": 3}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::Twice",
                expected: json!([
                    {"name": "pb::Twice", "edges": [
                        {"symbol": {"name": "pb::Scale", "filePath": "src/varint.cc", "startLine": 5}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::PutText",
                expected: json!([
                    {"name": "pb::PutText", "edges": [
                        {"symbol": {"name": "pb::Put", "filePath": "src/varint.cc", "startLine": 8}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "WriteCount",
                expected: json!([
                    {"name": "WriteCount", "edges": [
                        {"symbol": {"name": "write", "filePath": "src/fmtlike.h", "startLine": 5}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "WriteLong",
                expected: json!([
                    {"name": "WriteLong", "edges": [
                        {"symbol": {"name": "ns::Serialize", "filePath": "src/serialize.h", "startLine": 4}},
                    ]},
                ]),
                ..Query::default()
            },
            // Disabled: Shape converts to neither Circle nor Square; no Render overload matches.
            // Query {
            //     operation: "callees",
            //     symbol: "pb::Draw",
            //     expected: json!([
            //         {"name": "pb::Draw", "edges": [
            //             {"symbol": {"name": "pb::Render", "filePath": "src/render.cc", "startLine": 4}},
            //         ]},
            //     ]),
            //     ..Query::default()
            // },
            Query {
                operation: "callees",
                symbol: "a::Hello",
                expected: json!([
                    {"name": "a::Hello", "edges": [
                        {"symbol": {"name": "a::Print", "filePath": "src/print.cc", "startLine": 3}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "encode",
                expected: json!([
                    {"filePath": "src/rust/upb/wire.rs", "edges": [
                        {"symbol": {"name": "upb_encode", "filePath": "src/rust/upb/sys/wire/wire.rs", "startLine": 2}},
                    ]},
                ]),
                file: Some("src/rust/upb/wire.rs"),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "LogCode",
                expected: json!([
                    {"name": "LogCode", "edges": [
                        {"symbol": {"name": "lg::Log", "filePath": "src/log.cc", "startLine": 3}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "WarnCode",
                expected: json!([
                    {"name": "WarnCode", "edges": [
                        {"symbol": {"name": "lg::Warn", "filePath": "src/log.cc", "startLine": 4}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// rust-self-owner.test.ts:26
#[tokio::test]
async fn rust_missing_self_method_does_not_borrow_another_modules_owner() {
    check_case(Case {
        name: "rust_missing_self_method_does_not_borrow_another_modules_owner",
        files: vec![
            SourceFile {
                path: "Cargo.toml",
                source: "[package]\nname=\"owners\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
            },
            SourceFile {
                path: "src/lib.rs",
                source: "pub mod caller; pub mod decoy;",
            },
            SourceFile {
                path: "src/caller.rs",
                source: "pub struct Target;\nimpl Target { pub fn run(&self) { self.reset(); } }",
            },
            SourceFile {
                path: "src/decoy.rs",
                source: "pub struct Target;\nimpl Target { pub fn reset(&self) {} }",
            },
        ],
        queries: vec![Query {
            operation: "callees",
            symbol: "run",
            expected: json!([
                {"edges": [
                ]},
            ]),
            file: Some("src/caller.rs"),
            ..Query::default()
        }],
    })
    .await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// rust-self-owner.test.ts:35
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn rust_local_self_owner_survives_remove_and_restore() {
    check_stages(
        &[
            Case {
                name: "rust_local_self_owner_survives_remove_and_restore",
                files: vec![
                    SourceFile {
                        path: "Cargo.toml",
                        source: "[package]\nname=\"owners\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
                    },
                    SourceFile {
                        path: "src/lib.rs",
                        source: "pub mod caller; pub mod decoy;",
                    },
                    SourceFile {
                        path: "src/caller.rs",
                        source: "pub struct Target;\nimpl Target { pub fn reset(&self) {} }\nimpl Target { pub fn run(&self) { self.reset(); } }",
                    },
                    SourceFile {
                        path: "src/decoy.rs",
                        source: "pub struct Target;\nimpl Target { pub fn reset(&self) {} }",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callees",
                        symbol: "run",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "Target::reset", "filePath": "src/caller.rs"}},
                            ]},
                        ]),
                        file: Some("src/caller.rs"),
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "rust_local_self_owner_survives_remove_and_restore",
                files: vec![
                    SourceFile {
                        path: "Cargo.toml",
                        source: "[package]\nname=\"owners\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
                    },
                    SourceFile {
                        path: "src/lib.rs",
                        source: "pub mod caller; pub mod decoy;",
                    },
                    SourceFile {
                        path: "src/caller.rs",
                        source: "pub struct Target;\nimpl Target { pub fn run(&self) { self.reset(); } }",
                    },
                    SourceFile {
                        path: "src/decoy.rs",
                        source: "pub struct Target;\nimpl Target { pub fn reset(&self) {} }",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callees",
                        symbol: "run",
                        expected: json!([
                            {"edges": [
                            ]},
                        ]),
                        file: Some("src/caller.rs"),
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "rust_local_self_owner_survives_remove_and_restore",
                files: vec![
                    SourceFile {
                        path: "Cargo.toml",
                        source: "[package]\nname=\"owners\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
                    },
                    SourceFile {
                        path: "src/lib.rs",
                        source: "pub mod caller; pub mod decoy;",
                    },
                    SourceFile {
                        path: "src/caller.rs",
                        source: "pub struct Target;\nimpl Target { pub fn reset(&self) {} }\nimpl Target { pub fn run(&self) { self.reset(); } }",
                    },
                    SourceFile {
                        path: "src/decoy.rs",
                        source: "pub struct Target;\nimpl Target { pub fn reset(&self) {} }",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callees",
                        symbol: "run",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "Target::reset", "filePath": "src/caller.rs"}},
                            ]},
                        ]),
                        file: Some("src/caller.rs"),
                        ..Query::default()
                    },
                ],
            },
        ],
        false,
    ).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// rust-self-owner.test.ts:50
#[tokio::test]
async fn rust_unique_owner_can_have_impls_in_different_files() {
    check_case(Case {
        name: "rust_unique_owner_can_have_impls_in_different_files",
        files: vec![
            SourceFile {
                path: "Cargo.toml",
                source: "[package]\nname=\"owners\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
            },
            SourceFile {
                path: "src/lib.rs",
                source: "pub mod caller;\npub struct Target;\nimpl Target { pub fn reset(&self) {} }",
            },
            SourceFile {
                path: "src/caller.rs",
                source: "use crate::Target;\nimpl Target { pub fn run(&self) { self.reset(); } }",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Target::reset", "filePath": "src/lib.rs"}},
                    ]},
                ]),
                file: Some("src/caller.rs"),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// rust-self-owner.test.ts:58
#[tokio::test]
async fn rust_inline_modules_do_not_share_same_named_owners() {
    check_case(Case {
        name: "rust_inline_modules_do_not_share_same_named_owners",
        files: vec![
            SourceFile {
                path: "Cargo.toml",
                source: "[package]\nname=\"owners\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
            },
            SourceFile {
                path: "src/lib.rs",
                source: "mod a {\n    pub struct Target;\n    impl Target { pub fn reset(&self) {} }\n  }\n  mod b {\n    pub struct Target;\n    impl Target { pub fn run(&self) { self.reset(); } }\n  }",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                file: Some("src/lib.rs"),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// rust-go-call-shape.test.ts:83
// rust-go-call-shape.test.ts:90
#[tokio::test]
async fn rust_rust_chain_keeps_project_method_without_guessing_std_receiver() {
    check_case(Case {
        name: "rust_rust_chain_keeps_project_method_without_guessing_std_receiver",
        files: vec![
            SourceFile {
                path: "Cargo.toml",
                source: "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
            },
            SourceFile {
                path: "src/guard.rs",
                source: "pub struct MutexGuard;\nimpl MutexGuard {\n    pub fn map(self) -> Self { self }\n}\npub struct CookieJar;\nimpl CookieJar {\n    pub fn get(&self) -> Option<u8> { None }\n}\npub struct Arg;\nimpl Arg {\n    pub fn short(self, c: char) -> Self { self }\n}\npub fn flag(name: &str) -> Arg { Arg }\n",
            },
            SourceFile {
                path: "src/main.rs",
                source: "mod guard;\nuse guard::flag;\nfn get(handler: u8) -> u8 { handler }\nfn run(sym: Option<&str>) {\n    let _ = sym.map(|s| s.len());\n    let _ = get(1);\n    let _ = flag(\"n\").short('n');\n}\nfn main() { run(None); }\n",
            },
            SourceFile {
                path: "go.mod",
                source: "module example.com/app\n\ngo 1.21\n",
            },
            SourceFile {
                path: "flags.go",
                source: "package app\n\ntype customString struct{}\n\nfunc (c customString) String() string { return \"\" }\n\ntype FlagSet struct{}\n\nfunc Flags() *FlagSet { return nil }\n",
            },
            SourceFile {
                path: "cmd.go",
                source: "package app\n\nfunc run() {\n\tFlags().String(\"name\", \"\", \"\")\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "MutexGuard::map"}},
                        {"symbol": {"name": "CookieJar::get"}},
                    ]},
                ]),
                file: Some("src/main.rs"),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Arg::short"}},
                    ]},
                ]),
                file: Some("src/main.rs"),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "customString::String"}},
                    ]},
                ]),
                file: Some("cmd.go"),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-absl-holder-receiver.test.ts:72 — is the held class's method for `->` on an absl::StatusOr, absl::optional or absl::NoDestructor
#[tokio::test]
async fn cpp_absl_holder_receiver_upstream_72() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_absl_holder_receiver_upstream_72",
        files: vec![
            SourceFile {
                path: "src/feature_resolver.h",
                source: "namespace google {\nnamespace protobuf {\nclass FeatureResolver {\n public:\n  int MergeFeatures(int parent, int child) const { return parent; }\n};\nclass FeatureSetResolver {\n public:\n  int MergeFeatures(int parent, int child) const { return child; }\n};\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/feature_resolver_test.cc",
                source: "#include \"src/feature_resolver.h\"\nnamespace google {\nnamespace protobuf {\nint Merge(absl::StatusOr<FeatureResolver> resolver) {\n  return resolver->MergeFeatures(1, 2);\n}\nint MergeOptional(absl::optional<FeatureResolver> resolver) {\n  return resolver->MergeFeatures(1, 2);\n}\nint MergeStatic() {\n  static absl::NoDestructor<FeatureResolver> resolver;\n  return resolver->MergeFeatures(1, 2);\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::Merge",
                expected: json!([
                    {"name": "google::protobuf::Merge", "edges": [
                        {"symbol": {"name": "google::protobuf::FeatureResolver::MergeFeatures"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::MergeOptional",
                expected: json!([
                    {"name": "google::protobuf::MergeOptional", "edges": [
                        {"symbol": {"name": "google::protobuf::FeatureResolver::MergeFeatures"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::MergeStatic",
                expected: json!([
                    {"name": "google::protobuf::MergeStatic", "edges": [
                        {"symbol": {"name": "google::protobuf::FeatureResolver::MergeFeatures"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-absl-holder-receiver.test.ts:134 — is the member of the base the using-declaration names, of two dependent bases
#[tokio::test]
async fn cpp_absl_holder_receiver_upstream_134() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_absl_holder_receiver_upstream_134",
        files: vec![
            SourceFile {
                path: "util/runner.h",
                source: "namespace rocksdb {\ntemplate <class T>\nclass Second {\n public:\n  void Run() {}\n};\ntemplate <class T>\nclass First {\n public:\n  void Run() {}\n};\ntemplate <class T>\nclass Both : public Second<T>, public First<T> {\n public:\n  using First<T>::Run;\n  void Go() { Run(); }\n};\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Both::Go",
                expected: json!([
                    {"name": "rocksdb::Both::Go", "edges": [
                        {"symbol": {"name": "rocksdb::First::Run"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-absl-holder-receiver.test.ts:164 — is the dependent base member a using-declaration brings in, for a bare call and a data member
#[tokio::test]
async fn cpp_absl_holder_receiver_upstream_164() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_absl_holder_receiver_upstream_164",
        files: vec![
            SourceFile {
                path: "cache/typed_cache_base.h",
                source: "namespace rocksdb {\nclass Cache {\n public:\n  int Insert(int key) { return key; }\n};\nclass Other {\n public:\n  int Insert(int key) { return 0; }\n};\ntemplate <class TValue>\nclass BasicTypedCacheHelperFns {\n public:\n  static TValue* DownCastValue(void* v) { return nullptr; }\n protected:\n  Cache* cache_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "cache/typed_cache.h",
                source: "#include \"cache/typed_cache_base.h\"\nnamespace rocksdb {\ninline int* DownCastValue(void* v) { return nullptr; }\ntemplate <class TValue, class TCreateContext>\nclass FullTypedCacheHelperFns : public BasicTypedCacheHelperFns<TValue> {\n public:\n  using BasicTypedCacheHelperFns<TValue>::DownCastValue;\n  using BasicTypedCacheHelperFns<TValue>::cache_;\n  static void Delete(void* v) {\n    DownCastValue(v);\n  }\n  int Store(int key) {\n    return cache_->Insert(key);\n  }\n};\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::FullTypedCacheHelperFns::Delete",
                expected: json!([
                    {"name": "rocksdb::FullTypedCacheHelperFns::Delete", "edges": [
                        {"symbol": {"name": "rocksdb::BasicTypedCacheHelperFns::DownCastValue"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::FullTypedCacheHelperFns::Store",
                expected: json!([
                    {"name": "rocksdb::FullTypedCacheHelperFns::Store", "edges": [
                        {"symbol": {"name": "rocksdb::Cache::Insert"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-local-receiver.test.ts:113 — reproduction: a smart pointer cast to, and what a method is declared to return
#[tokio::test]
async fn cpp_auto_local_receiver_upstream_113() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_local_receiver_upstream_113",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator_base.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual ~IteratorBase() {}\n  virtual bool Valid() const = 0;\n  virtual void SeekToFirst() = 0;\n  virtual void Next() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "#include \"include/rocksdb/iterator_base.h\"\nnamespace rocksdb {\nclass Iterator : public IteratorBase {\n public:\n  virtual int value() const = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/memtablerep.h",
                source: "namespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    virtual bool Valid() const = 0;\n    virtual void SeekToFirst() = 0;\n    virtual void Next() = 0;\n  };\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "include/rocksdb/db.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DB {\n public:\n  virtual Iterator* NewIterator() = 0;\n};\nclass Transaction {\n public:\n  virtual Iterator* GetIterator() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_iterator_test.cc",
                source: "#include <memory>\n#include \"include/rocksdb/db.h\"\n#include \"include/rocksdb/memtablerep.h\"\nnamespace rocksdb {\nint CountKeys(DB* db) {\n  int n = 0;\n  auto iter = std::unique_ptr<Iterator>(db->NewIterator());\n  for (iter->SeekToFirst(); iter->Valid(); iter->Next()) n += iter->value();\n  return n;\n}\nbool FirstValid(Transaction* txn) {\n  auto it = txn->GetIterator();\n  it->SeekToFirst();\n  return it->Valid();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::CountKeys",
                expected: json!([
                    {"name": "rocksdb::CountKeys", "edges": [
                        {"symbol": {"name": "rocksdb::DB::NewIterator"}},
                        {"symbol": {"name": "rocksdb::Iterator::value"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::Next"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::SeekToFirst"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::FirstValid",
                expected: json!([
                    {"name": "rocksdb::FirstValid", "edges": [
                        {"symbol": {"name": "rocksdb::IteratorBase::SeekToFirst"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::Valid"}},
                        {"symbol": {"name": "rocksdb::Transaction::GetIterator"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-local-receiver.test.ts:154 — a smart pointer to a class template of the project's, whatever its arguments
#[tokio::test]
async fn cpp_auto_local_receiver_upstream_154() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_local_receiver_upstream_154",
        files: vec![
            SourceFile {
                path: "table/external_table.cc",
                source: "#include <memory>\nnamespace rocksdb {\ntemplate <int Mode>\nclass ExternalTableReaderBase {\n public:\n  virtual ~ExternalTableReaderBase() {}\n  int NewIterator() { return Mode; }\n};\nclass IndexFactoryReader {\n public:\n  int NewIterator() { return 0; }\n};\ntemplate <int Mode>\nclass ExternalTableReaderAdapter {\n public:\n  int Open() {\n    auto reader = std::unique_ptr<ExternalTableReaderBase<Mode>>(nullptr);\n    return reader->NewIterator();\n  }\n};\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::ExternalTableReaderAdapter::Open",
                expected: json!([
                    {"name": "rocksdb::ExternalTableReaderAdapter::Open", "edges": [
                        {"symbol": {"name": "rocksdb::ExternalTableReaderBase::NewIterator"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-local-receiver.test.ts:188 — a cast, and a construction through a typedef
#[tokio::test]
async fn cpp_auto_local_receiver_upstream_188() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_local_receiver_upstream_188",
        files: vec![
            SourceFile {
                path: "monitoring/instrumented_mutex.h",
                source: "namespace rocksdb {\nclass InstrumentedMutex {\n public:\n  void Lock() {}\n  void Unlock() {}\n};\nnamespace port {\nclass Mutex {\n public:\n  void Lock() {}\n  void Unlock() {}\n};\n}  // namespace port\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "memtable/skiplist.h",
                source: "namespace rocksdb {\ntemplate <typename Key, class Comparator>\nclass SkipList {\n public:\n  void Insert(const Key& key) {}\n};\nclass InlineSkipList {\n public:\n  void Insert(const char* key) {}\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/merge_test.cc",
                source: "#include \"monitoring/instrumented_mutex.h\"\n#include \"memtable/skiplist.h\"\nnamespace rocksdb {\nvoid Callback(void* arg) {\n  auto* mutex = static_cast<InstrumentedMutex*>(arg);\n  mutex->Unlock();\n  mutex->Lock();\n}\nclass HashSkipListRep {\n public:\n  typedef SkipList<const char*, int> Bucket;\n  void Add(const char* key) {\n    auto list = new Bucket();\n    list->Insert(key);\n  }\n};\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Callback",
                expected: json!([
                    {"name": "rocksdb::Callback", "edges": [
                        {"symbol": {"name": "rocksdb::InstrumentedMutex::Lock"}},
                        {"symbol": {"name": "rocksdb::InstrumentedMutex::Unlock"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::HashSkipListRep::Add",
                expected: json!([
                    {"name": "rocksdb::HashSkipListRep::Add", "edges": [
                        {"symbol": {"name": "rocksdb::SkipList::Insert"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-local-receiver.test.ts:249 — a bare call to a method the caller's class inherits, and a smart pointer made for a class
#[tokio::test]
async fn cpp_auto_local_receiver_upstream_249() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_local_receiver_upstream_249",
        files: vec![
            SourceFile {
                path: "src/google/protobuf/micro_string.h",
                source: "#include <memory>\nnamespace google {\nnamespace protobuf {\nstruct LargeRep {\n  void ChangeSize(int n) {}\n};\nstruct MicroRep {\n  void ChangeSize(int n) {}\n};\nclass MicroStringBase {\n protected:\n  MicroRep* micro_rep() { return nullptr; }\n  LargeRep* large_rep() { return nullptr; }\n};\nclass MicroString : public MicroStringBase {\n public:\n  void Grow();\n  void Clear();\n};\nnamespace ui {\nclass Widget {\n public:\n  void Draw() {}\n};\n}  // namespace ui\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/google/protobuf/micro_string.cc",
                source: "#include \"src/google/protobuf/micro_string.h\"\nnamespace google {\nnamespace protobuf {\nvoid MicroString::Grow() {\n  LargeRep* h = large_rep();\n  h->ChangeSize(2);\n}\nnamespace test {\nclass Widget {\n public:\n  void Draw() {}\n};\n}  // namespace test\nvoid MicroString::Clear() {\n  auto* h = micro_rep();\n  h->ChangeSize(0);\n}\nnamespace ui {\nvoid Paint() {\n  auto w = std::make_unique<Widget>();\n  w->Draw();\n}\nvoid Repaint() {\n  auto w = Widget();\n  w.Draw();\n}\n}  // namespace ui\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::MicroString::Clear",
                expected: json!([
                    {"name": "google::protobuf::MicroString::Clear", "edges": [
                        {"symbol": {"name": "google::protobuf::MicroRep::ChangeSize"}},
                        {"symbol": {"name": "google::protobuf::MicroStringBase::micro_rep"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::ui::Paint",
                expected: json!([
                    {"name": "google::protobuf::ui::Paint", "edges": [
                        {"symbol": {"name": "google::protobuf::ui::Widget::Draw"}},
                    ]},
                ]),
                edge_filters: vec![("includes", "Draw", false)],
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::ui::Repaint",
                expected: json!([
                    {"name": "google::protobuf::ui::Repaint", "edges": [
                        {"symbol": {"name": "google::protobuf::ui::Widget::Draw"}},
                    ]},
                ]),
                edge_filters: vec![("includes", "Draw", false)],
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-local-receiver.test.ts:326 — a chained call through a smart pointer reaches a method of the class it holds
#[tokio::test]
async fn cpp_auto_local_receiver_upstream_326() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_local_receiver_upstream_326",
        files: vec![
            SourceFile {
                path: "table/table_test.cc",
                source: "#include <memory>\nnamespace rocksdb {\nclass PinnedReader {\n public:\n  void SetPinnedData(int data) {}\n};\nclass OtherReader {\n public:\n  void SetPinnedData(int data) {}\n};\nclass PinnedFactory {\n public:\n  PinnedReader* last_reader() const { return nullptr; }\n};\nvoid Pin() {\n  auto factory = std::make_shared<PinnedFactory>();\n  factory->last_reader()->SetPinnedData(1);\n}\nvoid PinAgain(const std::unique_ptr<PinnedFactory>& factory) {\n  factory->last_reader()->SetPinnedData(2);\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Pin",
                expected: json!([
                    {"name": "rocksdb::Pin", "edges": [
                        {"symbol": {"name": "rocksdb::PinnedFactory::last_reader"}},
                        {"symbol": {"name": "rocksdb::PinnedReader::SetPinnedData"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::PinAgain",
                expected: json!([
                    {"name": "rocksdb::PinAgain", "edges": [
                        {"symbol": {"name": "rocksdb::PinnedFactory::last_reader"}},
                        {"symbol": {"name": "rocksdb::PinnedReader::SetPinnedData"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-local-receiver.test.ts:361 — the type of a local a chained call reads first is the same for its other calls
#[tokio::test]
async fn cpp_auto_local_receiver_upstream_361() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_local_receiver_upstream_361",
        files: vec![
            SourceFile {
                path: "db/version_set.h",
                source: "#include <memory>\nnamespace rocksdb {\nclass VersionStorageInfo {\n public:\n  int LevelFiles(int level) const { return level; }\n};\nclass Version {\n public:\n  VersionStorageInfo* storage_info() { return nullptr; }\n};\nclass ColumnFamilyData {\n public:\n  Version* current() const { return nullptr; }\n};\nclass ColumnFamilySet {\n public:\n  ColumnFamilyData* GetDefault() const { return nullptr; }\n};\nclass VersionSet {\n public:\n  ColumnFamilySet* GetColumnFamilySet() { return nullptr; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/compaction/compaction_job_test.cc",
                source: "#include \"db/version_set.h\"\nnamespace rocksdb {\nclass CompactionJobTestBase {\n public:\n  void VerifyTables(int output_level);\n protected:\n  std::unique_ptr<VersionSet> versions_;\n};\nvoid CompactionJobTestBase::VerifyTables(int output_level) {\n  auto cfd = versions_->GetColumnFamilySet()->GetDefault();\n  cfd->current()->storage_info()->LevelFiles(output_level);\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::CompactionJobTestBase::VerifyTables",
                expected: json!([
                    {"name": "rocksdb::CompactionJobTestBase::VerifyTables", "edges": [
                        {"symbol": {"name": "rocksdb::ColumnFamilyData::current"}},
                        {"symbol": {"name": "rocksdb::ColumnFamilySet::GetDefault"}},
                        {"symbol": {"name": "rocksdb::Version::storage_info"}},
                        {"symbol": {"name": "rocksdb::VersionSet::GetColumnFamilySet"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-local-receiver.test.ts:418 — a chained call reaches the class the inner call is declared to return, not a namesake
#[tokio::test]
async fn cpp_auto_local_receiver_upstream_418() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_local_receiver_upstream_418",
        files: vec![
            SourceFile {
                path: "src/google/protobuf/compiler/csharp/csharp_enum.h",
                source: "namespace google {\nnamespace protobuf {\nnamespace compiler {\nnamespace csharp {\nclass EnumGenerator {\n public:\n  void Generate(int printer) {}\n};\n}  // namespace csharp\n}  // namespace compiler\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/google/protobuf/compiler/java/generator_factory.h",
                source: "namespace google {\nnamespace protobuf {\nnamespace compiler {\nnamespace java {\nclass EnumGenerator {\n public:\n  void Generate(int printer) {}\n};\nclass GeneratorFactory {\n public:\n  EnumGenerator* NewEnumGenerator(int descriptor) const { return nullptr; }\n};\n}  // namespace java\n}  // namespace compiler\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/google/protobuf/compiler/java/file.cc",
                source: "#include \"src/google/protobuf/compiler/java/generator_factory.h\"\nnamespace google {\nnamespace protobuf {\nnamespace compiler {\nnamespace java {\nvoid GenerateEnums(const GeneratorFactory* generator_factory) {\n  generator_factory->NewEnumGenerator(1)->Generate(2);\n}\n}  // namespace java\n}  // namespace compiler\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::compiler::java::GenerateEnums",
                expected: json!([
                    {"name": "google::protobuf::compiler::java::GenerateEnums", "edges": [
                        {"symbol": {"name": "google::protobuf::compiler::java::EnumGenerator::Generate"}},
                        {"symbol": {"name": "google::protobuf::compiler::java::GeneratorFactory::NewEnumGenerator"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-local-receiver.test.ts:480 — an initializer that goes on past its line is read to its end
#[tokio::test]
async fn cpp_auto_local_receiver_upstream_480() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_local_receiver_upstream_480",
        files: vec![
            SourceFile {
                path: "include/rocksdb/merge_operator.h",
                source: "#include <memory>\nnamespace rocksdb {\nclass Customizable {\n public:\n  template <typename T>\n  T* CheckedCast() { return nullptr; }\n};\nclass MergeOperator : public Customizable {\n public:\n  virtual const char* Name() const { return \"\"; }\n};\nstruct ColumnFamilyOptions {\n  std::shared_ptr<MergeOperator> merge_operator;\n};\nnamespace test {\nclass ChanglingMergeOperator : public MergeOperator {\n public:\n  void SetName(const char* name) {}\n};\n}  // namespace test\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "options/options_test.cc",
                source: "#include \"include/rocksdb/merge_operator.h\"\nnamespace rocksdb {\nvoid Rename(ColumnFamilyOptions* base_cf_opt) {\n  auto* merge_operator = base_cf_opt->merge_operator\n                             ->CheckedCast<test::ChanglingMergeOperator>();\n  merge_operator->SetName(\"other\");\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Rename",
                expected: json!([
                    {"name": "rocksdb::Rename", "edges": [
                        {"symbol": {"name": "rocksdb::test::ChanglingMergeOperator::SetName"}},
                    ]},
                ]),
                edge_filters: vec![("includes", "SetName", false)],
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-local-receiver.test.ts:526 — what a function template returns is a template parameter, after the macros that follow its header
#[tokio::test]
async fn cpp_auto_local_receiver_upstream_526() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_local_receiver_upstream_526",
        files: vec![
            SourceFile {
                path: "src/google/protobuf/arena.h",
                source: "namespace google {\nnamespace protobuf {\nclass Arena {\n public:\n  void Reset() {}\n  template <typename T, typename... Args>\n  PROTOBUF_FUTURE_ADD_EARLY_NODISCARD PROTOBUF_NDEBUG_INLINE static T*\n      PROTOBUF_NONNULL\n      Create(Arena* arena, Args&&... args) {\n    return new T();\n  }\n};\ntemplate <typename Element>\nclass RepeatedPtrField {\n public:\n  Element* Add() { return nullptr; }\n};\nclass MessageLite {\n public:\n  int AccessCachedSize() const { return 0; }\n};\nclass Message : public MessageLite {};\nclass Pool {\n public:\n  void Reset() {}\n};\nPROTOBUF_EXPORT Arena* PROTOBUF_NONNULL DefaultArena() { return nullptr; }\n// Overloads by argument: what each returns depends on T.\ntemplate <typename T>\nPROTOBUF_FUTURE_ADD_EARLY_NODISCARD std::shared_ptr<T> DownCastMessage(const std::shared_ptr<MessageLite>& from) {\n  return nullptr;\n}\ntemplate <typename T>\nPROTOBUF_FUTURE_ADD_EARLY_NODISCARD const T& DownCastMessage(const MessageLite& from) {\n  return *static_cast<const T*>(&from);\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/google/protobuf/repeated_ptr_field_unittest.cc",
                source: "#include \"src/google/protobuf/arena.h\"\nnamespace google {\nnamespace protobuf {\nvoid Fill(Arena* arena) {\n  auto* field = Arena::Create<RepeatedPtrField<int>>(arena);\n  field->Add();\n}\nint ByteSizeLong(const MessageLite& msg) {\n  auto& _this = DownCastMessage<Message>(msg);\n  return _this.AccessCachedSize();\n}\nvoid Clear() {\n  auto* arena = DefaultArena();\n  arena->Reset();\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::Fill",
                expected: json!([
                    {"name": "google::protobuf::Fill", "edges": [
                        {"symbol": {"name": "google::protobuf::RepeatedPtrField::Add"}},
                    ]},
                ]),
                edge_filters: vec![("includes", "::Add", false)],
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::ByteSizeLong",
                expected: json!([
                    {"name": "google::protobuf::ByteSizeLong", "edges": [
                        {"symbol": {"name": "google::protobuf::MessageLite::AccessCachedSize"}},
                    ]},
                ]),
                edge_filters: vec![("includes", "AccessCachedSize", false)],
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::Clear",
                expected: json!([
                    {"name": "google::protobuf::Clear", "edges": [
                        {"symbol": {"name": "google::protobuf::Arena::Reset"}},
                    ]},
                ]),
                edge_filters: vec![("includes", "::Reset", false)],
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-local-receiver.test.ts:604 — one under a namespace the project declares nothing in
#[tokio::test]
async fn cpp_auto_local_receiver_upstream_604() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_local_receiver_upstream_604",
        files: vec![
            SourceFile {
                path: "upb/status.h",
                source: "namespace upb {\nclass Status {\n public:\n  bool ok() const { return true; }\n};\n}  // namespace upb\n",
            },
            SourceFile {
                path: "conformance/conformance_test.cc",
                source: "#include \"absl/status/status.h\"\n#include \"upb/status.h\"\nnamespace google {\nnamespace protobuf {\nbool Passed(const absl::Status& status) {\n  return status.ok();\n}\nabsl::Status Parse(int payload) { return absl::Status(); }\nbool Parsed(int payload) {\n  return Parse(payload).ok();\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/google/protobuf/lite_unittest.cc",
                source: "#include \"absl/strings/cord.h\"\nnamespace google {\nnamespace protobuf {\ntemplate <typename T>\nT SerializeAs(const int& msg);\ntemplate <>\nabsl::Cord SerializeAs<absl::Cord>(const int& msg) {\n  return absl::Cord();\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::Passed",
                expected: json!([
                    {"name": "google::protobuf::Passed", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::Parsed",
                expected: json!([
                    {"name": "google::protobuf::Parsed", "edges": [
                    ]},
                ]),
                edge_filters: vec![("includes", "::ok", false)],
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-local-receiver.test.ts:653 — a standard one whose name a class of the project shares
#[tokio::test]
async fn cpp_auto_local_receiver_upstream_653() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_local_receiver_upstream_653",
        files: vec![
            SourceFile {
                path: "include/fmt/os.h",
                source: "#define FMT_BEGIN_NAMESPACE namespace fmt { inline namespace v11 {\n#define FMT_END_NAMESPACE } }\nFMT_BEGIN_NAMESPACE\nclass ostream {\n public:\n  void flush() {}\n};\nFMT_END_NAMESPACE\n",
            },
            SourceFile {
                path: "src/ostream.cc",
                source: "#include <ostream>\n#include \"include/fmt/os.h\"\nFMT_BEGIN_NAMESPACE\nvoid flush_stream(std::ostream& os) {\n  os.flush();\n}\nFMT_END_NAMESPACE\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "flush_stream",
                expected: json!([
                    {"name": "flush_stream", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-local-receiver.test.ts:683 — not one of a namespace the project declares, which the file does not see
#[tokio::test]
async fn cpp_auto_local_receiver_upstream_683() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_local_receiver_upstream_683",
        files: vec![
            SourceFile {
                path: "src/google/protobuf/descriptor.pb.h",
                source: "namespace google {\nnamespace protobuf {\nclass FileDescriptorProto {\n public:\n  void Clear() {}\n};\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/google/protobuf/compiler/java/shared_code_generator.cc",
                source: "namespace google {\nnamespace protobuf {\nvoid Reset(google::protobuf::FileDescriptorProto& file_proto) {\n  file_proto.Clear();\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::Reset",
                expected: json!([
                    {"name": "google::protobuf::Reset", "edges": [
                        {"symbol": {"name": "google::protobuf::FileDescriptorProto::Clear"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-range-for-receiver.test.ts:254 — reproduction: rocksdb's `for (auto cfd : cfds)` over an autovector parameter
// cpp-auto-range-for-receiver.test.ts:265 — a project class iterated through its begin(), reached from a member another header declares
#[tokio::test]
async fn cpp_auto_range_for_receiver_upstream_254() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_range_for_receiver_upstream_254",
        files: vec![
            SourceFile {
                path: "util/autovector.h",
                source: "#include <cstddef>\nnamespace rocksdb {\ntemplate <class T, size_t kSize = 8>\nclass autovector {\n public:\n  size_t size() const { return 0; }\n  bool empty() const { return true; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/column_family.h",
                source: "#include <cstdint>\nnamespace rocksdb {\nclass ColumnFamilyData {\n public:\n  bool IsDropped() const;\n  void Ref();\n  uint32_t GetID() const;\n};\nclass ColumnFamilySet {\n public:\n  class iterator {\n   public:\n    explicit iterator(ColumnFamilyData* cfd) : current_(cfd) {}\n    iterator& operator++() { current_ = nullptr; return *this; }\n    bool operator!=(const iterator& other) const { return current_ != other.current_; }\n    ColumnFamilyData* operator*() { return current_; }\n   private:\n    ColumnFamilyData* current_;\n  };\n  iterator begin() { return iterator(head_); }\n  iterator end() { return iterator(nullptr); }\n private:\n  ColumnFamilyData* head_ = nullptr;\n};\nclass ColumnFamilyHandle {\n public:\n  bool IsDropped() const;\n  void Ref();\n  uint32_t GetID() const;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/column_family.cc",
                source: "#include \"db/column_family.h\"\nnamespace rocksdb {\nbool ColumnFamilyData::IsDropped() const { return false; }\nvoid ColumnFamilyData::Ref() {}\nuint32_t ColumnFamilyData::GetID() const { return 0; }\nbool ColumnFamilyHandle::IsDropped() const { return false; }\nvoid ColumnFamilyHandle::Ref() {}\nuint32_t ColumnFamilyHandle::GetID() const { return 0; }\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/version_set.h",
                source: "#include <memory>\n#include \"db/column_family.h\"\nnamespace rocksdb {\nclass VersionSet {\n public:\n  ColumnFamilySet* GetColumnFamilySet() { return column_family_set_.get(); }\n private:\n  std::unique_ptr<ColumnFamilySet> column_family_set_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_impl.h",
                source: "#include <memory>\n#include \"util/autovector.h\"\n#include \"db/version_set.h\"\nnamespace rocksdb {\nclass DBImpl {\n public:\n  void AssignAtomicFlushSeq(const autovector<ColumnFamilyData*>& cfds);\n  void ScheduleFlushes();\n private:\n  std::unique_ptr<VersionSet> versions_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_impl_write.cc",
                source: "#include \"db/db_impl.h\"\nnamespace rocksdb {\nvoid UseHandle(ColumnFamilyHandle* cfd) {\n  cfd->Ref();\n}\nvoid DBImpl::AssignAtomicFlushSeq(const autovector<ColumnFamilyData*>& cfds) {\n  for (auto cfd : cfds) {\n    cfd->GetID();\n  }\n}\nvoid DBImpl::ScheduleFlushes() {\n  for (auto cfd : *versions_->GetColumnFamilySet()) {\n    if (!cfd->IsDropped()) cfd->Ref();\n  }\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::DBImpl::AssignAtomicFlushSeq",
                expected: json!([
                    {"name": "rocksdb::DBImpl::AssignAtomicFlushSeq", "edges": [
                        {"symbol": {"name": "rocksdb::ColumnFamilyData::GetID"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::UseHandle",
                expected: json!([
                    {"name": "rocksdb::UseHandle", "edges": [
                        {"symbol": {"name": "rocksdb::ColumnFamilyHandle::Ref"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::DBImpl::ScheduleFlushes",
                expected: json!([
                    {"name": "rocksdb::DBImpl::ScheduleFlushes", "edges": [
                        {"symbol": {"name": "rocksdb::ColumnFamilyData::IsDropped"}},
                        {"symbol": {"name": "rocksdb::ColumnFamilyData::Ref"}},
                    ]},
                ]),
                edge_filters: vec![("startsWith", "rocksdb::VersionSet::", true)],
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-range-for-receiver.test.ts:281 — a call whose declaration writes its return type, and a nullability annotation in an element
#[tokio::test]
async fn cpp_auto_range_for_receiver_upstream_281() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_range_for_receiver_upstream_281",
        files: vec![
            SourceFile {
                path: "src/google/protobuf/descriptor.h",
                source: "#include <string>\nnamespace google {\nnamespace protobuf {\nclass FieldDescriptor {\n public:\n  int number() const;\n  const std::string& full_name() const;\n};\nclass Descriptor {\n public:\n  int field_count() const;\n};\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/google/protobuf/descriptor.cc",
                source: "#include \"google/protobuf/descriptor.h\"\nnamespace google {\nnamespace protobuf {\nint FieldDescriptor::number() const { return 0; }\nconst std::string& FieldDescriptor::full_name() const { static std::string s; return s; }\nint Descriptor::field_count() const { return 0; }\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "upb/reflection/def.hpp",
                source: "namespace upb {\nclass FieldDefPtr {\n public:\n  int number() const { return 0; }\n  const char* full_name() const { return \"\"; }\n};\n}  // namespace upb\n",
            },
            SourceFile {
                path: "src/google/protobuf/compiler/cpp/field_layout.h",
                source: "#include <vector>\n#include \"absl/types/span.h\"\n#include \"google/protobuf/descriptor.h\"\nnamespace google {\nnamespace protobuf {\nnamespace compiler {\nnamespace cpp {\nstd::vector<const FieldDescriptor*> FieldNumberOrder(const Descriptor* descriptor);\nclass FieldLayout {\n public:\n  absl::Span<const FieldDescriptor* const absl_nonnull> optimized_order() const { return {}; }\n};\n}  // namespace cpp\n}  // namespace compiler\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/google/protobuf/compiler/cpp/message.cc",
                source: "#include \"google/protobuf/compiler/cpp/field_layout.h\"\nnamespace google {\nnamespace protobuf {\nnamespace compiler {\nnamespace cpp {\nstd::vector<const FieldDescriptor*> FieldNumberOrder(const Descriptor* descriptor) {\n  return {};\n}\nint Numbers(const Descriptor* descriptor) {\n  int sum = 0;\n  for (const auto* field : FieldNumberOrder(descriptor)) {\n    sum += field->number();\n  }\n  return sum;\n}\nvoid Names(const FieldLayout& layout) {\n  for (const auto* field : layout.optimized_order()) {\n    field->full_name();\n  }\n}\n}  // namespace cpp\n}  // namespace compiler\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::compiler::cpp::Numbers",
                expected: json!([
                    {"name": "google::protobuf::compiler::cpp::Numbers", "edges": [
                        {"symbol": {"name": "google::protobuf::FieldDescriptor::number"}},
                        {"symbol": {"name": "google::protobuf::compiler::cpp::FieldNumberOrder"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::compiler::cpp::Names",
                expected: json!([
                    {"name": "google::protobuf::compiler::cpp::Names", "edges": [
                        {"symbol": {"name": "google::protobuf::FieldDescriptor::full_name"}},
                        {"symbol": {"name": "google::protobuf::compiler::cpp::FieldLayout::optimized_order"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-range-for-receiver.test.ts:297 — an `auto` local's initializer, an alias with template arguments, and a smart pointer element
#[tokio::test]
async fn cpp_auto_range_for_receiver_upstream_297() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_range_for_receiver_upstream_297",
        files: vec![
            SourceFile {
                path: "db/blob_file_meta.h",
                source: "#include <cstdint>\nnamespace rocksdb {\nclass BlobFileMetaData {\n public:\n  uint64_t GetBlobFileNumber() const { return 0; }\n};\nclass BlobFileAddition {\n public:\n  uint64_t GetBlobFileNumber() const { return 0; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/version_storage.h",
                source: "#include <memory>\n#include <vector>\n#include \"db/blob_file_meta.h\"\nnamespace rocksdb {\nclass VersionStorageInfo {\n public:\n  using BlobFiles = std::vector<std::shared_ptr<BlobFileMetaData>>;\n  const BlobFiles& GetBlobFiles() const { return blob_files_; }\n private:\n  BlobFiles blob_files_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_filesnapshot.cc",
                source: "#include \"db/version_storage.h\"\nnamespace rocksdb {\nuint64_t Largest(const VersionStorageInfo* vstorage) {\n  uint64_t largest = 0;\n  const auto& blob_files = vstorage->GetBlobFiles();\n  for (const auto& meta : blob_files) {\n    largest = meta->GetBlobFileNumber();\n  }\n  return largest;\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Largest",
                expected: json!([
                    {"name": "rocksdb::Largest", "edges": [
                        {"symbol": {"name": "rocksdb::BlobFileMetaData::GetBlobFileNumber"}},
                        {"symbol": {"name": "rocksdb::VersionStorageInfo::GetBlobFiles"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-range-for-receiver.test.ts:351 — a data member of another object, and loops inside loops
#[tokio::test]
async fn cpp_auto_range_for_receiver_upstream_351() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_range_for_receiver_upstream_351",
        files: vec![
            SourceFile {
                path: "db/compaction/subcompaction_state.h",
                source: "namespace rocksdb {\nclass Output {\n public:\n  void Finish() {}\n};\nclass SubcompactionState {\n public:\n  void RemoveLastEmptyOutput() {}\n  Output outputs[2];\n};\nclass FlushJob {\n public:\n  void RemoveLastEmptyOutput() {}\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/compaction/compaction_state.h",
                source: "#include <vector>\n#include \"db/compaction/subcompaction_state.h\"\nnamespace rocksdb {\nclass CompactionState {\n public:\n  std::vector<SubcompactionState> sub_compact_states;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/compaction/compaction_job.h",
                source: "#include \"db/compaction/compaction_state.h\"\nnamespace rocksdb {\nclass CompactionJob {\n public:\n  void RemoveEmptyOutputs();\n private:\n  CompactionState* compact_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/compaction/compaction_job.cc",
                source: "#include \"db/compaction/compaction_job.h\"\nnamespace rocksdb {\nvoid CompactionJob::RemoveEmptyOutputs() {\n  for (auto& state : compact_->sub_compact_states) {\n    state.RemoveLastEmptyOutput();\n  }\n}\nvoid Finish(const std::vector<CompactionState*>& compactions) {\n  for (const auto* compaction : compactions) {\n    for (auto& state : compaction->sub_compact_states) {\n      state.RemoveLastEmptyOutput();\n    }\n  }\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::CompactionJob::RemoveEmptyOutputs",
                expected: json!([
                    {"name": "rocksdb::CompactionJob::RemoveEmptyOutputs", "edges": [
                        {"symbol": {"name": "rocksdb::SubcompactionState::RemoveLastEmptyOutput"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::Finish",
                expected: json!([
                    {"name": "rocksdb::Finish", "edges": [
                        {"symbol": {"name": "rocksdb::SubcompactionState::RemoveLastEmptyOutput"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-range-for-receiver.test.ts:441 — a standard-library element's own member links to nothing
#[tokio::test]
async fn cpp_auto_range_for_receiver_upstream_441() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_range_for_receiver_upstream_441",
        files: vec![
            SourceFile {
                path: "util/slice.h",
                source: "#include <cstddef>\nnamespace rocksdb {\nclass Slice {\n public:\n  size_t size() const { return 0; }\n  const char* c_str() const { return \"\"; }\n};\nclass TableReader {\n public:\n  bool Valid() const { return true; }\n  void reset() {}\n};\nclass TruncatedRangeDelIterator {\n public:\n  bool Valid() const { return true; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/names.cc",
                source: "#include <string>\n#include <thread>\n#include <vector>\n#include \"util/slice.h\"\nnamespace rocksdb {\nsize_t Total(const std::vector<std::string>& files) {\n  size_t total = 0;\n  for (const auto& file : files) {\n    total += file.size();\n  }\n  return total;\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Total",
                expected: json!([
                    {"name": "rocksdb::Total", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-auto-range-for-receiver.test.ts:468 — a smart pointer element: `->` reaches the type it holds, `.` the holder's own member
#[tokio::test]
async fn cpp_auto_range_for_receiver_upstream_468() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_auto_range_for_receiver_upstream_468",
        files: vec![
            SourceFile {
                path: "util/slice.h",
                source: "#include <cstddef>\nnamespace rocksdb {\nclass Slice {\n public:\n  size_t size() const { return 0; }\n  const char* c_str() const { return \"\"; }\n};\nclass TableReader {\n public:\n  bool Valid() const { return true; }\n  void reset() {}\n};\nclass TruncatedRangeDelIterator {\n public:\n  bool Valid() const { return true; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/range_del_aggregator.h",
                source: "#include <memory>\n#include <vector>\n#include \"util/slice.h\"\nnamespace rocksdb {\nclass RangeDelAggregator {\n public:\n  bool IsRangeOverlapped();\n  void Reset();\n private:\n  std::vector<std::unique_ptr<TruncatedRangeDelIterator>> iters_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/range_del_aggregator.cc",
                source: "#include \"db/range_del_aggregator.h\"\nnamespace rocksdb {\nbool RangeDelAggregator::IsRangeOverlapped() {\n  for (auto& iter : iters_) {\n    if (iter->Valid()) return true;\n  }\n  return false;\n}\nvoid RangeDelAggregator::Reset() {\n  for (auto& iter : iters_) iter.reset();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::RangeDelAggregator::IsRangeOverlapped",
                expected: json!([
                    {"name": "rocksdb::RangeDelAggregator::IsRangeOverlapped", "edges": [
                        {"symbol": {"name": "rocksdb::TruncatedRangeDelIterator::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:58 — reproduction: the macro from an included header does not bind to the decoy function
#[tokio::test]
async fn cpp_macro_and_constructor_calls_upstream_58() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_upstream_58",
        files: vec![
            SourceFile {
                path: "marker.hpp",
                source: "#define TRACE_POINT(value) ((void)(value))\n",
            },
            SourceFile {
                path: "exercise.cpp",
                source: "#include \"marker.hpp\"\n\nvoid exercise() {\n    TRACE_POINT(1);\n}\n",
            },
            SourceFile {
                path: "macro_decoy.cpp",
                source: "void TRACE_POINT(int value) {}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "exercise",
                expected: json!([
                    {"name": "exercise", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "TRACE_POINT",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    })
    .await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:144 — a diamond-shaped, cyclic include graph still finds the macro on an unconditional path
#[tokio::test]
async fn cpp_macro_and_constructor_calls_upstream_144() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_upstream_144",
        files: vec![
            SourceFile {
                path: "top.h",
                source: "#include \"left.h\"\n#include \"right.h\"\n",
            },
            SourceFile {
                path: "left.h",
                source: "#ifdef USE_LEFT\n#include \"shared.h\"\n#endif\n",
            },
            SourceFile {
                path: "right.h",
                source: "#include \"shared.h\"\n",
            },
            SourceFile {
                path: "shared.h",
                source: "#include \"top.h\"\n#define SHARED_TRACE(v) ((void)(v))\n",
            },
            SourceFile {
                path: "unit.cpp",
                source: "#include \"top.h\"\nvoid unit() { SHARED_TRACE(1); }\n",
            },
            SourceFile {
                path: "decoy.cpp",
                source: "void SHARED_TRACE(int value) {}\n",
            },
        ],
        queries: vec![Query {
            operation: "callees",
            symbol: "unit",
            expected: json!([
                {"name": "unit", "edges": [
                ]},
            ]),
            ..Query::default()
        }],
    })
    .await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:163 — control: a #define inside a block comment, string or raw literal is not a macro
#[tokio::test]
async fn cpp_macro_and_constructor_calls_upstream_163() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_upstream_163",
        files: vec![
            SourceFile {
                path: "doc.hpp",
                source: "/*\n * Example:\n * #define helper(x) ((x) + 1)\n */\nconst char *usage = \"/* #define helper(x) */\";\nconst char *raw = R\"doc(\n#define helper(x) ((x) + 9)\n)doc\";\n",
            },
            SourceFile {
                path: "lib.cpp",
                source: "#include \"doc.hpp\"\nint helper(int x) { return x + 1; }\nint run() { return helper(1); }\n",
            },
            SourceFile {
                path: "other.cpp",
                source: "#define helper(x) ((x) + 2)\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"name": "run", "edges": [
                        {"symbol": {"name": "helper", "filePath": "lib.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:176 — control: a wrapper macro whose body calls the same-named function keeps that call
#[tokio::test]
async fn cpp_macro_and_constructor_calls_upstream_176() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_upstream_176",
        files: vec![
            SourceFile {
                path: "vec.c",
                source: "static void vec_splice(char **data, int start, int count) {}\n#define vec_splice(v, start, count)\\\n  ( vec_splice((char **)(v), start, count),\\\n    (v)->length -= (count) )\nstruct buf { char *data; int length; };\nvoid flush(struct buf *b) { vec_splice(b, 0, 1); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "flush",
                expected: json!([
                    {"name": "flush", "edges": [
                        {"symbol": {"name": "vec_splice", "filePath": "vec.c"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:195 — control: a macro defined only in an unrelated file neither suppresses nor receives a real call
#[tokio::test]
async fn cpp_macro_and_constructor_calls_upstream_195() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_upstream_195",
        files: vec![
            SourceFile {
                path: "unrelated.hpp",
                source: "#define helper(x) ((x) + 1)\n",
            },
            SourceFile {
                path: "lib.cpp",
                source: "int helper(int x) { return x + 1; }\n",
            },
            SourceFile {
                path: "main.cpp",
                source: "int helper(int x);\nint run() { return helper(1); }\n",
            },
        ],
        queries: vec![Query {
            operation: "callees",
            symbol: "run",
            expected: json!([
                {"name": "run", "edges": [
                    {"symbol": {"name": "helper", "filePath": "lib.cpp"}},
                ]},
            ]),
            ..Query::default()
        }],
    })
    .await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:208 — control: a call above the #define, or under a known-false branch, is still a call
#[tokio::test]
async fn cpp_macro_and_constructor_calls_upstream_208() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_upstream_208",
        files: vec![
            SourceFile {
                path: "maths.h",
                source: "#define FAST_MATH\n#if defined(FAST_MATH)\nfloat sin_approx(float x);\n#else\n#define sin_approx(x) external_sin(x)\n#endif\n",
            },
            SourceFile {
                path: "maths.cpp",
                source: "#include \"maths.h\"\nfloat sin_approx(float x) { return x; }\nvoid real_fn(int value) {}\nfloat invoke_real(float value) { real_fn(1); return sin_approx(value); }\n#define real_fn(x) ((void)(x))\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "invoke_real",
                expected: json!([
                    {"name": "invoke_real", "edges": [
                        {"symbol": {"name": "real_fn", "filePath": "maths.cpp"}},
                        {"symbol": {"name": "sin_approx", "filePath": "maths.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:293 — reproduction: every initialization form reaches the indexed constructor
// the aggregate reaches none
// cpp-macro-and-constructor-calls.test.ts:453 (constructor retrieval
// text labels not exposed)
#[tokio::test]
async fn cpp_macro_and_constructor_calls_upstream_293() {
    // Signatures identify the constructor definitions on lines 11 and 12; kind and instantiation labels are not exposed by zg.
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_upstream_293",
        files: vec![
            SourceFile {
                path: "case.cpp",
                source: "struct Aggregate {\n    int value;\n};\n\nclass WithConstructor {\npublic:\n    WithConstructor();\n    explicit WithConstructor(int value);\n};\n\nWithConstructor::WithConstructor() {}\nWithConstructor::WithConstructor(int value) {}\n\nvoid aggregate_initialization() {\n    Aggregate item{};\n}\n\nvoid constructor_default() {\n    WithConstructor item;\n}\n\nvoid constructor_braced() {\n    WithConstructor item{};\n}\n\nvoid constructor_value() {\n    WithConstructor item(1);\n}\n\nvoid constructor_temporary() {\n    WithConstructor();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "aggregate_initialization",
                expected: json!([
                    {"name": "aggregate_initialization", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "constructor_default",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "WithConstructor::WithConstructor", "startLine": 11}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "constructor_braced",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "WithConstructor::WithConstructor", "startLine": 11}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "constructor_value",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "WithConstructor::WithConstructor", "startLine": 12}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "constructor_temporary",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                edge_filters: vec![("startsWith", "WithConstructor::WithConstructor", true)],
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:324 — picks the constructor in the lexical namespace of the site, then an enclosing/global type
#[tokio::test]
async fn cpp_macro_and_constructor_calls_upstream_324() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_upstream_324",
        files: vec![
            SourceFile {
                path: "ns.cpp",
                source: "struct Global { Global() {} };\nunion Value { Value() {} int x; };\nvoid union_use() { Value v; }\nnamespace first { struct Widget { Widget() {} }; }\nnamespace second {\n  struct Widget { Widget() {} };\n  void local_use() { Widget w; }\n  void global_use() { Global g; }\n}\nvoid explicit_use() { first::Widget w; }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "union_use",
                expected: json!([
                    {"name": "union_use", "edges": [
                        {"symbol": {"name": "Value::Value", "filePath": "ns.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "local_use",
                expected: json!([
                    {"name": "local_use", "edges": [
                        {"symbol": {"name": "second::Widget::Widget", "filePath": "ns.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "global_use",
                expected: json!([
                    {"name": "global_use", "edges": [
                        {"symbol": {"name": "Global::Global", "filePath": "ns.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "explicit_use",
                expected: json!([
                    {"name": "explicit_use", "edges": [
                        {"symbol": {"name": "first::Widget::Widget", "filePath": "ns.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:386 — controls: pointers, references, prototypes and extern declarations construct no object
// arrays construct elements
#[tokio::test]
async fn cpp_macro_and_constructor_calls_upstream_386() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_upstream_386",
        files: vec![
            SourceFile {
                path: "controls.cpp",
                source: "struct Widget {\n  Widget() {}\n  Widget(int) {}\n};\nvoid pointers() { Widget *p{}; Widget *q(nullptr); Widget *arr[2]{}; Widget (*fn)(){}; }\nvoid reference_bind(Widget &other) { Widget &r{other}; Widget &s(other); }\nvoid prototype() { Widget most_vexing(); extern Widget external; }\nvoid array() { Widget items[2]{}; }\nvoid actual() { Widget object{}; }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "pointers",
                expected: json!([
                    {"name": "pointers", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "reference_bind",
                expected: json!([
                    {"name": "reference_bind", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "prototype",
                expected: json!([
                    {"name": "prototype", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array",
                expected: json!([
                    {"name": "array", "edges": [
                        {"symbol": {"name": "Widget::Widget", "filePath": "controls.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "actual",
                expected: json!([
                    {"name": "actual", "edges": [
                        {"symbol": {"name": "Widget::Widget", "filePath": "controls.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:412 — merges separately declared defaults with definitions without confusing same-arity overloads
#[tokio::test]
async fn cpp_macro_and_constructor_calls_upstream_412() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_upstream_412",
        files: vec![
            SourceFile {
                path: "widget.hpp",
                source: "namespace app {\nstruct Widget {\n  Widget(int value = 7);\n  Widget(double value);\n};\nstruct Pair { Pair(int first, int second = 2); };\n}\n",
            },
            SourceFile {
                path: "widget.cpp",
                source: "#include \"widget.hpp\"\napp::Widget::Widget(int renamed) {}\napp::Widget::Widget(double renamed) {}\napp::Pair::Pair(int a, int b) {}\n",
            },
            SourceFile {
                path: "use.cpp",
                source: "#include \"widget.hpp\"\nint argument(int value) { return value; }\nvoid defaults() { app::Widget item; }\nvoid nested() { app::Pair item(argument(1)); }\nvoid ambiguous() { app::Widget item(1); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "defaults",
                expected: json!([
                    {"name": "defaults", "edges": [
                        {"symbol": {"name": "app::Widget::Widget", "filePath": "widget.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "nested",
                expected: json!([
                    {"name": "nested", "edges": [
                        {"symbol": {"name": "argument", "filePath": "use.cpp"}},
                        {"symbol": {"name": "app::Pair::Pair", "filePath": "widget.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ambiguous",
                expected: json!([
                    {"name": "ambiguous", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "app::Widget::Widget",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "defaults"}},
                    ]},
                ]),
                file: Some("widget.cpp"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-defined-methods.test.ts:109 — reproduction: protobuf descriptor accessors, and calls through what they return
#[tokio::test]
async fn cpp_macro_defined_methods_upstream_109() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_macro_defined_methods_upstream_109",
        files: vec![
            SourceFile {
                path: "src/legacy.h",
                source: "namespace legacy {\nclass Descriptor {\n public:\n  absl::string_view full_name() const { return \"\"; }\n};\n}  // namespace legacy\n",
            },
            SourceFile {
                path: "src/descriptor.h",
                source: "namespace google {\nnamespace protobuf {\nclass FieldDescriptor;\nclass Descriptor {\n public:\n  absl::string_view name() const;\n  absl::string_view full_name() const;\n  int field_count() const;\n  const FieldDescriptor* field(int index) const;\n  const Descriptor* containing_type() const;\n private:\n  int field_count_;\n  const FieldDescriptor* fields_;\n  const Descriptor* containing_type_;\n};\nclass FieldDescriptor {\n public:\n  int number() const;\n  const Descriptor* containing_type() const;\n private:\n  int number_;\n  const Descriptor* containing_type_;\n};\n\n#define PROTOBUF_DEFINE_ACCESSOR(CLASS, FIELD, TYPE) \\\n  inline TYPE CLASS::FIELD() const { return FIELD##_; }\n\n// Name and full name are stored in a single array to save space.\n#define PROTOBUF_DEFINE_NAME_ACCESSOR(CLASS)                                 \\\n  inline absl::string_view CLASS::name() const { return all_names_.name(); } \\\n  inline absl::string_view CLASS::full_name() const {                        \\\n    return all_names_.full_name();                                           \\\n  }\n\n#define PROTOBUF_DEFINE_ARRAY_ACCESSOR(CLASS, FIELD, TYPE) \\\n  inline TYPE CLASS::FIELD(int index) const {              \\\n    return FIELD##s_ + index;                              \\\n  }\n\nPROTOBUF_DEFINE_NAME_ACCESSOR(Descriptor)\nPROTOBUF_DEFINE_ACCESSOR(Descriptor, field_count, int)\nPROTOBUF_DEFINE_ARRAY_ACCESSOR(Descriptor, field, const FieldDescriptor*)\nPROTOBUF_DEFINE_ACCESSOR(Descriptor, containing_type,\n                         const Descriptor*)\nPROTOBUF_DEFINE_ACCESSOR(FieldDescriptor, number, int)\nPROTOBUF_DEFINE_ACCESSOR(FieldDescriptor, containing_type, const Descriptor*)\n\n#undef PROTOBUF_DEFINE_ACCESSOR\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/printer.cc",
                source: "#include \"src/legacy.h\"\n#include \"src/descriptor.h\"\nnamespace google {\nnamespace protobuf {\nint Number(const FieldDescriptor* field) { return field->number(); }\nint Count(const Descriptor& d) { return d.field_count(); }\nabsl::string_view Owner(const FieldDescriptor* field) { return field->containing_type()->full_name(); }\nint First(const Descriptor* d) { return d->field(0)->number(); }\nabsl::string_view Parent(const FieldDescriptor* field) {\n  auto parent = field->containing_type();\n  return parent->full_name();\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::Number",
                expected: json!([
                    {"name": "google::protobuf::Number", "edges": [
                        {"symbol": {"name": "google::protobuf::FieldDescriptor::number", "filePath": "src/descriptor.h", "startLine": 45}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::Count",
                expected: json!([
                    {"name": "google::protobuf::Count", "edges": [
                        {"symbol": {"name": "google::protobuf::Descriptor::field_count", "filePath": "src/descriptor.h", "startLine": 41}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::Owner",
                expected: json!([
                    {"name": "google::protobuf::Owner", "edges": [
                        {"symbol": {"name": "google::protobuf::Descriptor::full_name", "filePath": "src/descriptor.h", "startLine": 40}},
                        {"symbol": {"name": "google::protobuf::FieldDescriptor::containing_type", "filePath": "src/descriptor.h", "startLine": 46}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::First",
                expected: json!([
                    {"name": "google::protobuf::First", "edges": [
                        {"symbol": {"name": "google::protobuf::Descriptor::field", "filePath": "src/descriptor.h", "startLine": 42}},
                        {"symbol": {"name": "google::protobuf::FieldDescriptor::number", "filePath": "src/descriptor.h", "startLine": 45}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::Parent",
                expected: json!([
                    {"name": "google::protobuf::Parent", "edges": [
                        {"symbol": {"name": "google::protobuf::Descriptor::full_name", "filePath": "src/descriptor.h", "startLine": 40}},
                        {"symbol": {"name": "google::protobuf::FieldDescriptor::containing_type", "filePath": "src/descriptor.h", "startLine": 46}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-defined-methods.test.ts:173 — a macro of a source file that pastes the method name, for a class its header declares
#[tokio::test]
async fn cpp_macro_defined_methods_upstream_173() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_macro_defined_methods_upstream_173",
        files: vec![
            SourceFile {
                path: "src/reflection.h",
                source: "class Reflection {\n public:\n  int GetInt32(int index) const;\n  void SetInt32(int index, int value) const;\n  bool GetBool(int index) const;\n  void SetBool(int index, bool value) const;\n};\n",
            },
            SourceFile {
                path: "src/reflection.cc",
                source: "#include \"src/reflection.h\"\n#define DEFINE_PRIMITIVE_ACCESSORS(TYPENAME, TYPE)          \\\n  TYPE Reflection::Get##TYPENAME(int index) const {         \\\n    return TYPE();                                          \\\n  }                                                         \\\n  void Reflection::Set##TYPENAME(int index, TYPE value) const {}\n\nDEFINE_PRIMITIVE_ACCESSORS(Int32, int)\nDEFINE_PRIMITIVE_ACCESSORS(Bool, bool)\n#undef DEFINE_PRIMITIVE_ACCESSORS\nnamespace {\nclass Local {\n public:\n  int Get() const;\n};\n#define DEFINE_LOCAL_GET(C) inline int C::Get() const { return 1; }\nDEFINE_LOCAL_GET(Local)\n}  // namespace\n",
            },
            SourceFile {
                path: "src/use.cc",
                source: "#include \"src/reflection.h\"\nint Read(const Reflection& r) { r.SetBool(0, true); return r.GetInt32(1); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Read",
                expected: json!([
                    {"name": "Read", "edges": [
                        {"symbol": {"name": "Reflection::GetInt32", "filePath": "src/reflection.cc", "startLine": 8}},
                        {"symbol": {"name": "Reflection::SetBool", "filePath": "src/reflection.cc", "startLine": 9}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-defined-methods.test.ts:224 — a macro-defined overload is picked by the parameters its expansion writes, and is never a deleted one
#[tokio::test]
async fn cpp_macro_defined_methods_upstream_224() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_macro_defined_methods_upstream_224",
        files: vec![
            SourceFile {
                path: "src/getter.h",
                source: "class Getter {\n public:\n  int Get(int index) const;\n  int Get(const char* key) const;\n};\n#define DEFINE_GET(PARAM) \\\n  inline int Getter::Get(PARAM) const { return 0; }\nDEFINE_GET(int index)\n// Between two invocations, another class deletes a method of the same name.\nclass Blocked {\n public:\n  int Get(double value) = delete;\n};\nDEFINE_GET(const char* key)\n",
            },
            SourceFile {
                path: "src/use.cc",
                source: "#include \"src/getter.h\"\nint ByKey(const Getter& g) { return g.Get(\"key\"); }\nint ByIndex(const Getter& g) { return g.Get(1); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "ByKey",
                expected: json!([
                    {"name": "ByKey", "edges": [
                        {"symbol": {"name": "Getter::Get", "filePath": "src/getter.h", "startLine": 14}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ByIndex",
                expected: json!([
                    {"name": "ByIndex", "edges": [
                        {"symbol": {"name": "Getter::Get", "filePath": "src/getter.h", "startLine": 8}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-member-receiver.test.ts:119 — reproduction: one the class declares in another header
#[tokio::test]
async fn cpp_member_receiver_upstream_119() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_member_receiver_upstream_119",
        files: vec![
            SourceFile {
                path: "db/version_set.h",
                source: "namespace rocksdb {\nclass ColumnFamilySet {\n public:\n  int NumberOfColumnFamilies() const { return 0; }\n};\nclass VersionSet {\n public:\n  ColumnFamilySet* GetColumnFamilySet() { return nullptr; }\n  void SetLastSequence(int s) {}\n};\nclass ColumnFamilyHandleImpl {\n public:\n  ColumnFamilySet* GetColumnFamilySet() { return nullptr; }\n  void SetLastSequence(int s) {}\n};\nclass Statistics {\n public:\n  bool load() const { return false; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "monitoring/instrumented_mutex.h",
                source: "namespace rocksdb {\nclass InstrumentedMutex {\n public:\n  void Lock() {}\n  void Unlock() {}\n};\nnamespace port {\nclass Mutex {\n public:\n  void Lock() {}\n  void Unlock() {}\n};\n}  // namespace port\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_impl/db_impl.h",
                source: "#include <atomic>\n#include <memory>\n#include \"db/version_set.h\"\n#include \"monitoring/instrumented_mutex.h\"\nnamespace rocksdb {\nclass DBImpl {\n public:\n  void WriteImpl();\n  void FlushMemTable();\n protected:\n  std::unique_ptr<VersionSet> versions_;\n  InstrumentedMutex mutex_;\n  std::atomic<bool> shutting_down_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_impl/db_impl_write.cc",
                source: "#include \"db/db_impl/db_impl.h\"\nnamespace rocksdb {\n// Another function's local of the name, which the method does not see.\nvoid UnlockStale(port::Mutex* mutex_) { mutex_->Unlock(); }\nvoid DBImpl::WriteImpl() {\n  mutex_.Lock();\n  versions_->SetLastSequence(1);\n  versions_->GetColumnFamilySet()->NumberOfColumnFamilies();\n  if (shutting_down_.load()) return;\n  mutex_.Unlock();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::DBImpl::WriteImpl",
                expected: json!([
                    {"name": "rocksdb::DBImpl::WriteImpl", "edges": [
                        {"symbol": {"name": "rocksdb::ColumnFamilySet::NumberOfColumnFamilies"}},
                        {"symbol": {"name": "rocksdb::InstrumentedMutex::Lock"}},
                        {"symbol": {"name": "rocksdb::InstrumentedMutex::Unlock"}},
                        {"symbol": {"name": "rocksdb::VersionSet::GetColumnFamilySet"}},
                        {"symbol": {"name": "rocksdb::VersionSet::SetLastSequence"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-member-receiver.test.ts:151 — one declared below the inline method that uses it, and one a base class declares
#[tokio::test]
async fn cpp_member_receiver_upstream_151() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_member_receiver_upstream_151",
        files: vec![
            SourceFile {
                path: "db/version_set.h",
                source: "namespace rocksdb {\nclass ColumnFamilySet {\n public:\n  int NumberOfColumnFamilies() const { return 0; }\n};\nclass VersionSet {\n public:\n  ColumnFamilySet* GetColumnFamilySet() { return nullptr; }\n  void SetLastSequence(int s) {}\n};\nclass ColumnFamilyHandleImpl {\n public:\n  ColumnFamilySet* GetColumnFamilySet() { return nullptr; }\n  void SetLastSequence(int s) {}\n};\nclass Statistics {\n public:\n  bool load() const { return false; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "monitoring/instrumented_mutex.h",
                source: "namespace rocksdb {\nclass InstrumentedMutex {\n public:\n  void Lock() {}\n  void Unlock() {}\n};\nnamespace port {\nclass Mutex {\n public:\n  void Lock() {}\n  void Unlock() {}\n};\n}  // namespace port\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_impl/db_impl.h",
                source: "#include <atomic>\n#include <memory>\n#include \"db/version_set.h\"\n#include \"monitoring/instrumented_mutex.h\"\nnamespace rocksdb {\nclass DBImpl {\n public:\n  void WriteImpl();\n  void FlushMemTable();\n protected:\n  std::unique_ptr<VersionSet> versions_;\n  InstrumentedMutex mutex_;\n  std::atomic<bool> shutting_down_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/memtable_list.h",
                source: "namespace rocksdb {\nclass MemTableListVersion {\n public:\n  int NumNotFlushed() const { return 0; }\n};\nclass MemTable {\n public:\n  int NumNotFlushed() const { return 1; }\n};\nclass MemTableList {\n public:\n  bool IsFlushPending() const { return current_->NumNotFlushed() > 0; }\n  // A line that starts with a dereference is code, not a comment.\n  int Pick(const int* first) const {\n    return Choose(0,\n                  *first);\n  }\n private:\n  int Choose(int a, int b) const { return a + b; }\n  /*\n  MemTable* current_;\n  */\n  MemTableListVersion* current_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_impl/db_impl_secondary.h",
                source: "#include \"db/db_impl/db_impl.h\"\nnamespace rocksdb {\nclass DBImplSecondary : public DBImpl {\n public:\n  void TryCatchUpWithPrimary();\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_impl/db_impl_secondary.cc",
                source: "#include \"db/db_impl/db_impl_secondary.h\"\nnamespace rocksdb {\nvoid DBImplSecondary::TryCatchUpWithPrimary() {\n  versions_->SetLastSequence(2);\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::MemTableList::IsFlushPending",
                expected: json!([
                    {"name": "rocksdb::MemTableList::IsFlushPending", "edges": [
                        {"symbol": {"name": "rocksdb::MemTableListVersion::NumNotFlushed"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::DBImplSecondary::TryCatchUpWithPrimary",
                expected: json!([
                    {"name": "rocksdb::DBImplSecondary::TryCatchUpWithPrimary", "edges": [
                        {"symbol": {"name": "rocksdb::VersionSet::SetLastSequence"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-member-receiver.test.ts:207 — a gtest TEST_F body's fixture member
#[tokio::test]
async fn cpp_member_receiver_upstream_207() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_member_receiver_upstream_207",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual void SeekToFirst() = 0;\n};\nclass Iterator : public IteratorBase {};\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    virtual void SeekToFirst() = 0;\n  };\n};\nclass DB {\n public:\n  virtual Iterator* NewIterator() = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_test_util.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DBTestBase {\n protected:\n  DB* db_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_iterator_test.cc",
                source: "#include \"db/db_test_util.h\"\nnamespace rocksdb {\nclass DBIteratorTest : public DBTestBase {};\nTEST_F(DBIteratorTest, IterSeekToFirst) {\n  auto iter = db_->NewIterator();\n  iter->SeekToFirst();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::TEST_F",
                expected: json!([
                    {"name": "rocksdb::TEST_F", "edges": [
                        {"symbol": {"name": "rocksdb::DB::NewIterator"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::SeekToFirst"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-member-receiver.test.ts:258 — not one of a class nested in it, which its header declares first
#[tokio::test]
async fn cpp_member_receiver_upstream_258() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_member_receiver_upstream_258",
        files: vec![
            SourceFile {
                path: "table/multi_scan.h",
                source: "namespace rocksdb {\nclass ScanOptions {\n public:\n  int size() const { return 0; }\n};\nclass ScanRanges {\n public:\n  int size() const { return 1; }\n};\nclass MultiScan {\n public:\n  class MultiScanIterator {\n   private:\n    ScanRanges* scan_opts_;\n  };\n  int Count() const;\n private:\n  ScanOptions* scan_opts_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "table/multi_scan.cc",
                source: "#include \"table/multi_scan.h\"\nnamespace rocksdb {\nint MultiScan::Count() const { return scan_opts_->size(); }\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::MultiScan::Count",
                expected: json!([
                    {"name": "rocksdb::MultiScan::Count", "edges": [
                        {"symbol": {"name": "rocksdb::ScanOptions::size"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-member-receiver.test.ts:402 — a parameter of a method the class declares, which is no member
#[tokio::test]
async fn cpp_member_receiver_upstream_402() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_member_receiver_upstream_402",
        files: vec![
            SourceFile {
                path: "db/version_set.h",
                source: "namespace rocksdb {\nclass ColumnFamilySet {\n public:\n  int NumberOfColumnFamilies() const { return 0; }\n};\nclass VersionSet {\n public:\n  ColumnFamilySet* GetColumnFamilySet() { return nullptr; }\n  void SetLastSequence(int s) {}\n};\nclass ColumnFamilyHandleImpl {\n public:\n  ColumnFamilySet* GetColumnFamilySet() { return nullptr; }\n  void SetLastSequence(int s) {}\n};\nclass Statistics {\n public:\n  bool load() const { return false; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "monitoring/instrumented_mutex.h",
                source: "namespace rocksdb {\nclass InstrumentedMutex {\n public:\n  void Lock() {}\n  void Unlock() {}\n};\nnamespace port {\nclass Mutex {\n public:\n  void Lock() {}\n  void Unlock() {}\n};\n}  // namespace port\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_impl/db_impl.h",
                source: "#include <atomic>\n#include <memory>\n#include \"db/version_set.h\"\n#include \"monitoring/instrumented_mutex.h\"\nnamespace rocksdb {\nclass DBImpl {\n public:\n  void WriteImpl();\n  void FlushMemTable();\n  void CreateColumnFamilies(std::vector<int>* column_families,\n                            int* handles);\n  int CountColumnFamilies();\n  ColumnFamilySet* stats() const;\n protected:\n  std::unique_ptr<VersionSet> versions_;\n  InstrumentedMutex mutex_;\n  std::atomic<bool> shutting_down_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_impl/db_impl_write.cc",
                source: "#include \"db/db_impl/db_impl.h\"\nnamespace rocksdb {\nint DBImpl::CountColumnFamilies() {\n  if (stats->load()) return 0;\n  return column_families->NumberOfColumnFamilies();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::DBImpl::CountColumnFamilies",
                expected: json!([
                    {"name": "rocksdb::DBImpl::CountColumnFamilies", "edges": [
                        {"symbol": {"name": "rocksdb::ColumnFamilySet::NumberOfColumnFamilies"}},
                    ]},
                ]),
                edge_filters: vec![("includes", "NumberOfColumnFamilies", false)],
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::DBImpl::CountColumnFamilies",
                expected: json!([
                    {"name": "rocksdb::DBImpl::CountColumnFamilies", "edges": [
                        {"symbol": {"name": "rocksdb::Statistics::load"}},
                    ]},
                ]),
                edge_filters: vec![("includes", "load", false)],
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-member-receiver.test.ts:433 — a member of a template parameter's type
#[tokio::test]
async fn cpp_member_receiver_upstream_433() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_member_receiver_upstream_433",
        files: vec![
            SourceFile {
                path: "util/comparator.cc",
                source: "namespace rocksdb {\nclass Comparator {\n public:\n  int timestamp_size() const { return 0; }\n};\ntemplate <typename TComparator>\nclass ComparatorWithU64TsImpl {\n public:\n  int Check() const { return cmp_without_ts_.timestamp_size(); }\n private:\n  TComparator cmp_without_ts_;\n};\n// rocksdb's clock cache test reaches its shard through a dependent alias.\nclass ClockCacheShard {\n public:\n  void Insert(int key) {}\n};\ntemplate <class ClockCache>\nclass ClockCacheTest {\n public:\n  using Shard = typename ClockCache::Shard;\n  void Insert(int key) { shard_->Insert(key); }\n private:\n  Shard* shard_ = nullptr;\n};\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::ComparatorWithU64TsImpl::Check",
                expected: json!([
                    {"name": "rocksdb::ComparatorWithU64TsImpl::Check", "edges": [
                        {"symbol": {"name": "rocksdb::Comparator::timestamp_size"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::ClockCacheTest::Insert",
                expected: json!([
                    {"name": "rocksdb::ClockCacheTest::Insert", "edges": [
                        {"symbol": {"name": "rocksdb::ClockCacheShard::Insert"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-member-receiver.test.ts:473 — a member a base class template declares through an alias of its parameter
#[tokio::test]
async fn cpp_member_receiver_upstream_473() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_member_receiver_upstream_473",
        files: vec![
            SourceFile {
                path: "src/google/protobuf/map_field.h",
                source: "#include <type_traits>\nnamespace google {\nnamespace protobuf {\nnamespace internal {\nclass MapFieldBase {\n public:\n  void SetMapDirty() {}\n};\n}  // namespace internal\ntemplate <bool kIsMutable>\nclass MapIteratorBase {\n protected:\n  using MapFieldBase = std::conditional_t<kIsMutable, internal::MapFieldBase, const internal::MapFieldBase>;\n  MapFieldBase* map_;\n};\nclass MapIterator : public MapIteratorBase<true> {\n public:\n  void MutableValueRef() { map_->SetMapDirty(); }\n};\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/google/protobuf/map_field.cc",
                source: "#include \"src/google/protobuf/map_field.h\"\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::MapIterator::MutableValueRef",
                expected: json!([
                    {"name": "google::protobuf::MapIterator::MutableValueRef", "edges": [
                        {"symbol": {"name": "google::protobuf::internal::MapFieldBase::SetMapDirty"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-nested-template-receiver.test.ts:118 — reproduction: a member of the class reaches the class's own method, not one named after the receiver's words
// cpp-nested-template-receiver.test.ts:130 — a local of a class template nested in its own template arguments
#[tokio::test]
async fn cpp_nested_template_receiver_upstream_118() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_nested_template_receiver_upstream_118",
        files: vec![
            SourceFile {
                path: "util/autovector.h",
                source: "namespace rocksdb {\ntemplate <class T, size_t kSize = 8>\nclass autovector {\n public:\n  size_t size() const { return 0; }\n  void clear() {}\n  void push_back(const T& item) {}\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/compaction/compaction.h",
                source: "namespace rocksdb {\nstruct FileMetaData {};\nstruct CompactionInputFiles {\n  size_t size() const { return 0; }\n  void clear() {}\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/version_set.h",
                source: "#include \"util/autovector.h\"\n#include \"db/compaction/compaction.h\"\nnamespace rocksdb {\nclass VersionEdit {};\nclass VersionStorageInfo {\n public:\n  void ComputeFilesMarkedForCompaction();\n  size_t CountEditLists(VersionEdit* edit);\n private:\n  autovector<std::pair<int, FileMetaData*>> files_marked_for_compaction_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/version_set.cc",
                source: "#include \"db/version_set.h\"\nnamespace rocksdb {\nvoid VersionStorageInfo::ComputeFilesMarkedForCompaction() {\n  files_marked_for_compaction_.clear();\n}\nsize_t VersionStorageInfo::CountEditLists(VersionEdit* edit) {\n  autovector<autovector<VersionEdit*>> edit_lists;\n  edit_lists.push_back(autovector<VersionEdit*>());\n  return edit_lists.size();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::VersionStorageInfo::ComputeFilesMarkedForCompaction",
                expected: json!([
                    {"name": "rocksdb::VersionStorageInfo::ComputeFilesMarkedForCompaction", "edges": [
                        {"symbol": {"name": "rocksdb::autovector::clear", "filePath": "util/autovector.h"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "rocksdb::CompactionInputFiles::clear",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::VersionStorageInfo::CountEditLists",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "rocksdb::autovector::push_back"}},
                        {"symbol": {"name": "rocksdb::autovector::size"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-nested-template-receiver.test.ts:145 — a capitalized class template is the project's own, not a type from outside it
#[tokio::test]
async fn cpp_nested_template_receiver_upstream_145() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_nested_template_receiver_upstream_145",
        files: vec![
            SourceFile {
                path: "util/mutexlock.h",
                source: "namespace rocksdb {\nnamespace port {\nclass Mutex {\n public:\n  void Lock() {}\n};\n}  // namespace port\ntemplate <class T>\nstruct CacheAlignedWrapper {\n  T obj_;\n};\ntemplate <class T, class Key = int>\nclass Striped {\n public:\n  T& Get(const Key& key) { return *data_; }\n private:\n  T* data_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/blob/blob_file_cache.h",
                source: "#include \"util/mutexlock.h\"\nnamespace rocksdb {\nclass BlobFileCache {\n public:\n  void Evict(int key);\n private:\n  Striped<CacheAlignedWrapper<port::Mutex>> mutex_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/blob/blob_file_cache.cc",
                source: "#include \"db/blob/blob_file_cache.h\"\nnamespace rocksdb {\nvoid BlobFileCache::Evict(int key) {\n  mutex_.Get(key);\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::BlobFileCache::Evict",
                expected: json!([
                    {"name": "rocksdb::BlobFileCache::Evict", "edges": [
                        {"symbol": {"name": "rocksdb::Striped::Get", "filePath": "util/mutexlock.h"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-nested-template-receiver.test.ts:238 — a declaration begun on the line above still hides a member of the same name
#[tokio::test]
async fn cpp_nested_template_receiver_upstream_238() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_nested_template_receiver_upstream_238",
        files: vec![
            SourceFile {
                path: "util/autovector.h",
                source: "namespace rocksdb {\ntemplate <class T, size_t kSize = 8>\nclass autovector {\n public:\n  size_t size() const { return 0; }\n  void clear() {}\n  void push_back(const T& item) {}\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/blob/blob_file_reader.cc",
                source: "#include <string>\n#include \"util/autovector.h\"\nnamespace rocksdb {\nstruct BlobContents {};\nclass BlobFileReader {\n public:\n  size_t MultiGetBlob(autovector<std::pair<int*,\n                                           std::unique_ptr<BlobContents>>>& blob_reqs) const;\n private:\n  std::string blob_reqs;\n};\nsize_t BlobFileReader::MultiGetBlob(autovector<std::pair<int*,\n                                           std::unique_ptr<BlobContents>>>& blob_reqs) const {\n  return blob_reqs.size();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::BlobFileReader::MultiGetBlob",
                expected: json!([
                    {"name": "rocksdb::BlobFileReader::MultiGetBlob", "edges": [
                        {"symbol": {"name": "rocksdb::autovector::size", "filePath": "util/autovector.h"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-qualified-declarator-receiver.test.ts:99 — reads `Iterator* const iter`, not an earlier function's `iter`
#[tokio::test]
async fn cpp_qualified_declarator_receiver_upstream_99() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_qualified_declarator_receiver_upstream_99",
        files: vec![
            SourceFile {
                path: "include/iterator.h",
                source: "namespace rocksdb {\nclass Iterator {\n public:\n  virtual ~Iterator() {}\n  virtual void SeekToFirst() = 0;\n  virtual bool Valid() const = 0;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/memtable.cc",
                source: "#include \"include/iterator.h\"\nnamespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    void SeekToFirst() {}\n    bool Valid() const { return false; }\n  };\n};\nvoid Earlier() {\n  MemTableRep::Iterator* iter = nullptr;\n  iter->SeekToFirst();\n}\nvoid Scan(Iterator* source) {\n  Iterator* const iter = source;\n  iter->SeekToFirst();\n  if (iter->Valid()) {}\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Scan",
                expected: json!([
                    {"name": "rocksdb::Scan", "edges": [
                        {"symbol": {"name": "rocksdb::Iterator::SeekToFirst"}},
                        {"symbol": {"name": "rocksdb::Iterator::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-qualified-declarator-receiver.test.ts:142 — reads a type written before its `const` (`Slice const& key`)
#[tokio::test]
async fn cpp_qualified_declarator_receiver_upstream_142() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_qualified_declarator_receiver_upstream_142",
        files: vec![
            SourceFile {
                path: "util/slice.h",
                source: "namespace leveldb {\nclass Slice {\n public:\n  int size() const { return 0; }\n};\nnamespace log {\nclass Slice {\n public:\n  int size() const { return 0; }\n};\n}  // namespace log\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/length.cc",
                source: "#include \"util/slice.h\"\nnamespace leveldb {\nint Length(Slice const& key) {\n  return key.size();\n}\nint Count(Slice const* key) {\n  return key->size();\n}\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::Length",
                expected: json!([
                    {"name": "leveldb::Length", "edges": [
                        {"symbol": {"name": "leveldb::Slice::size"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "leveldb::Count",
                expected: json!([
                    {"name": "leveldb::Count", "edges": [
                        {"symbol": {"name": "leveldb::Slice::size"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-qualified-declarator-receiver.test.ts:178 — reads past a nullability annotation after the `*` (`Arena* PROTOBUF_NULLABLE arena`)
#[tokio::test]
async fn cpp_qualified_declarator_receiver_upstream_178() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_qualified_declarator_receiver_upstream_178",
        files: vec![
            SourceFile {
                path: "src/google/protobuf/arena.h",
                source: "#define PROTOBUF_NULLABLE\n#define PROTOBUF_NONNULL\nnamespace google {\nnamespace protobuf {\nclass Arena {\n public:\n  void Reset() {}\n};\nclass ArenaStringPtr {\n public:\n  void Reset() {}\n};\nvoid Clear(Arena* PROTOBUF_NULLABLE arena) {\n  arena->Reset();\n}\nvoid ClearOwned(Arena* owner) {\n  Arena* absl_nonnull arena = owner;\n  arena->Reset();\n}\nclass Printer {\n public:\n  void Print() {}\n};\nclass Generator {\n public:\n  void Emit();\n private:\n  Printer* PROTOBUF_NONNULL printer_;\n};\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "src/google/protobuf/generator_emit.cc",
                source: "#include \"src/google/protobuf/arena.h\"\nnamespace google {\nnamespace protobuf {\nclass Formatter {\n public:\n  void Print() {}\n};\nvoid Generator::Emit() {\n  printer_->Print();\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::Clear",
                expected: json!([
                    {"name": "google::protobuf::Clear", "edges": [
                        {"symbol": {"name": "google::protobuf::Arena::Reset"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::ClearOwned",
                expected: json!([
                    {"name": "google::protobuf::ClearOwned", "edges": [
                        {"symbol": {"name": "google::protobuf::Arena::Reset"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "google::protobuf::Generator::Emit",
                expected: json!([
                    {"name": "google::protobuf::Generator::Emit", "edges": [
                        {"symbol": {"name": "google::protobuf::Printer::Print"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-qualified-declarator-receiver.test.ts:238 — reads a member with an annotation after its name (`VersionSet* const versions_ GUARDED_BY(mutex_);`)
#[tokio::test]
async fn cpp_qualified_declarator_receiver_upstream_238() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_qualified_declarator_receiver_upstream_238",
        files: vec![
            SourceFile {
                path: "db/version_set.h",
                source: "namespace leveldb {\nclass VersionSet {\n public:\n  int LogNumber() const { return 0; }\n};\nclass VersionEdit {\n public:\n  int LogNumber() const { return 0; }\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/db_impl.h",
                source: "#include \"db/version_set.h\"\n#define GUARDED_BY(x)\nnamespace leveldb {\nclass DBImpl {\n public:\n  void Recover();\n private:\n  VersionSet* const versions_ GUARDED_BY(mutex_);\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/db_impl_open.cc",
                source: "#include \"db/db_impl.h\"\nnamespace leveldb {\nvoid DBImpl::Recover() {\n  versions_->LogNumber();\n}\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::DBImpl::Recover",
                expected: json!([
                    {"name": "leveldb::DBImpl::Recover", "edges": [
                        {"symbol": {"name": "leveldb::VersionSet::LogNumber"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-qualified-declarator-receiver.test.ts:280 — reads `auto* const` and smart-pointer locals, and range-for variables
#[tokio::test]
async fn cpp_qualified_declarator_receiver_upstream_280() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_qualified_declarator_receiver_upstream_280",
        files: vec![
            SourceFile {
                path: "ui/widget.h",
                source: "namespace ui {\nclass Widget {\n public:\n  void Draw() {}\n};\n}  // namespace ui\n",
            },
            SourceFile {
                path: "ui/paint.cc",
                source: "#include <memory>\n#include <vector>\n#include \"ui/widget.h\"\n#define PROTOBUF_NONNULL\nnamespace legacy {\nclass Widget {\n public:\n  void Draw() {}\n};\n}  // namespace legacy\n\nnamespace ui {\nvoid Paint() {\n  auto* const w = new Widget();\n  w->Draw();\n}\nvoid PaintOwned(std::unique_ptr<Widget> const w) {\n  w->Draw();\n}\nvoid PaintAll(std::vector<Widget*>& ws) {\n  for (Widget* const w : ws) w->Draw();\n}\nvoid PaintEach(std::vector<std::unique_ptr<Widget>>& ws) {\n  for (std::unique_ptr<Widget> const& w : ws) w->Draw();\n}\nvoid PaintMarked(std::vector<Widget*>& ws) {\n  for (Widget* PROTOBUF_NONNULL w : ws) w->Draw();\n}\n}  // namespace ui\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "ui::Paint",
                expected: json!([
                    {"name": "ui::Paint", "edges": [
                        {"symbol": {"name": "ui::Widget::Draw"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ui::PaintOwned",
                expected: json!([
                    {"name": "ui::PaintOwned", "edges": [
                        {"symbol": {"name": "ui::Widget::Draw"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ui::PaintAll",
                expected: json!([
                    {"name": "ui::PaintAll", "edges": [
                        {"symbol": {"name": "ui::Widget::Draw"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ui::PaintEach",
                expected: json!([
                    {"name": "ui::PaintEach", "edges": [
                        {"symbol": {"name": "ui::Widget::Draw"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ui::PaintMarked",
                expected: json!([
                    {"name": "ui::PaintMarked", "edges": [
                        {"symbol": {"name": "ui::Widget::Draw"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-qualified-declarator-receiver.test.ts:320 — keeps an ALL-CAPS type name, which is no annotation before a `*`
#[tokio::test]
async fn cpp_qualified_declarator_receiver_upstream_320() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_qualified_declarator_receiver_upstream_320",
        files: vec![
            SourceFile {
                path: "handle.cc",
                source: "struct OS_HANDLE {\n  void Close() {}\n};\nstruct FILE_HANDLE {\n  void Close() {}\n};\nvoid Shut(OS_HANDLE* const h) {\n  h->Close();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Shut",
                expected: json!([
                    {"name": "Shut", "edges": [
                        {"symbol": {"name": "OS_HANDLE::Close"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-qualified-declarator-receiver.test.ts:341 — keeps the operator a qualified declaration takes: `.` on a reference, `->` on a pointer
#[tokio::test]
async fn cpp_qualified_declarator_receiver_upstream_341() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_qualified_declarator_receiver_upstream_341",
        files: vec![
            SourceFile {
                path: "ui/widget.h",
                source: "namespace ui {\nclass Widget {\n public:\n  void Draw() {}\n};\n}  // namespace ui\n",
            },
            SourceFile {
                path: "ui/draw.cc",
                source: "#include \"ui/widget.h\"\nnamespace legacy {\nclass Widget {\n public:\n  void Draw() {}\n};\n}  // namespace legacy\n\nnamespace ui {\nvoid ByRef(Widget const& w) { w.Draw(); }\nvoid ByPtr(Widget const* const w) { w->Draw(); }\n}  // namespace ui\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "ui::ByRef",
                expected: json!([
                    {"name": "ui::ByRef", "edges": [
                        {"symbol": {"name": "ui::Widget::Draw"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ui::ByPtr",
                expected: json!([
                    {"name": "ui::ByPtr", "edges": [
                        {"symbol": {"name": "ui::Widget::Draw"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-qualified-declarator-receiver.test.ts:361 — reads globals the same-named header declares with qualifiers
#[tokio::test]
async fn cpp_qualified_declarator_receiver_upstream_361() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_qualified_declarator_receiver_upstream_361",
        files: vec![
            SourceFile {
                path: "registry.h",
                source: "#define PROTOBUF_NULLABLE\nclass Registry {\n public:\n  void Add() {}\n};\nextern Registry* const g_registry;\nextern Registry* PROTOBUF_NULLABLE g_fallback;\n",
            },
            SourceFile {
                path: "registry.cc",
                source: "#include \"registry.h\"\nclass Catalog {\n public:\n  void Add() {}\n};\nvoid Use() {\n  g_registry->Add();\n}\nvoid UseFallback() {\n  g_fallback->Add();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Use",
                expected: json!([
                    {"name": "Use", "edges": [
                        {"symbol": {"name": "Registry::Add"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "UseFallback",
                expected: json!([
                    {"name": "UseFallback", "edges": [
                        {"symbol": {"name": "Registry::Add"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-qualified-declarator-receiver.test.ts:420 — reads a declaration without qualifiers as before
#[tokio::test]
async fn cpp_qualified_declarator_receiver_upstream_420() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_qualified_declarator_receiver_upstream_420",
        files: vec![
            SourceFile {
                path: "include/iterator.h",
                source: "namespace rocksdb {\nclass Iterator {\n public:\n  virtual ~Iterator() {}\n  virtual void SeekToFirst() = 0;\n  virtual bool Valid() const = 0;\n};\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    void SeekToFirst() {}\n    bool Valid() const { return false; }\n  };\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/plain.cc",
                source: "#include \"include/iterator.h\"\nnamespace rocksdb {\nvoid Plain(Iterator* iter) {\n  iter->SeekToFirst();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Plain",
                expected: json!([
                    {"name": "rocksdb::Plain", "edges": [
                        {"symbol": {"name": "rocksdb::Iterator::SeekToFirst"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-range-for-receiver.test.ts:188 — reproduction: protobuf's conformance runner calls the suite's methods through the typed path
#[tokio::test]
async fn cpp_range_for_receiver_upstream_188() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_range_for_receiver_upstream_188",
        files: vec![
            SourceFile {
                path: "conformance/suite.h",
                source: "#include <string>\nnamespace google {\nnamespace protobuf {\nclass ConformanceTestRunner {\n public:\n  virtual ~ConformanceTestRunner() = default;\n};\nclass ConformanceTestSuite {\n public:\n  void SetVerbose(bool verbose) { verbose_ = verbose; }\n  std::string GetFailureListFlagName() { return failure_list_flag_name_; }\n  bool RunSuite(ConformanceTestRunner* runner, std::string* output,\n                const std::string& filename);\n private:\n  bool verbose_ = false;\n  std::string failure_list_flag_name_;\n};\nclass FlagParser {\n public:\n  void SetVerbose(bool verbose) {}\n};\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "conformance/suite.cc",
                source: "#include \"conformance/suite.h\"\nnamespace google {\nnamespace protobuf {\nbool ConformanceTestSuite::RunSuite(ConformanceTestRunner* runner, std::string* output,\n                                    const std::string& filename) {\n  return true;\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "conformance/runner.cc",
                source: "#include <cstring>\n#include <vector>\n#include \"conformance/suite.h\"\nnamespace google {\nnamespace protobuf {\nint ForkPipeRunnerMain(int argc, char* argv[], const std::vector<ConformanceTestSuite*>& suites) {\n  bool verbose = false;\n  for (ConformanceTestSuite *suite : suites) {\n    std::string failure_list_filename;\n    for (int arg = 1; arg < argc; ++arg) {\n      if (strcmp(argv[arg], suite->GetFailureListFlagName().c_str()) == 0) {\n        failure_list_filename = argv[arg];\n      }\n    }\n    suite->SetVerbose(verbose);\n    std::string output;\n    ConformanceTestRunner* runner = nullptr;\n    verbose = verbose && suite->RunSuite(runner, &output,\n                                         failure_list_filename);\n  }\n  return 0;\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::ForkPipeRunnerMain",
                expected: json!([
                    {"name": "google::protobuf::ForkPipeRunnerMain", "edges": [
                        {"symbol": {"name": "google::protobuf::ConformanceTestSuite::GetFailureListFlagName"}},
                        {"symbol": {"name": "google::protobuf::ConformanceTestSuite::RunSuite"}},
                        {"symbol": {"name": "google::protobuf::ConformanceTestSuite::SetVerbose"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-range-for-receiver.test.ts:203 — a loop variable that hides a member is the element, not the member
#[tokio::test]
async fn cpp_range_for_receiver_upstream_203() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_range_for_receiver_upstream_203",
        files: vec![
            SourceFile {
                path: "garage/vehicle.h",
                source: "#include <vector>\nnamespace garage {\nclass Engine {\n public:\n  void Start();\n  int Torque() const;\n};\nclass Car {\n public:\n  void Start();\n};\nnamespace parts {\nclass Part {\n public:\n  void Fit();\n};\nvoid Install();\n}  // namespace parts\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/vehicle.cc",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nvoid Engine::Start() {}\nint Engine::Torque() const { return 0; }\nvoid Car::Start() {}\nvoid parts::Part::Fit() {}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.h",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nclass Garage {\n public:\n  void Inspect(const std::vector<Engine*>& engines);\n  void Service();\n private:\n  Car* car;\n};\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.cc",
                source: "#include \"garage/garage.h\"\nnamespace garage {\nvoid Garage::Inspect(const std::vector<Engine*>& engines) {\n  for (Engine* car : engines) car->Start();\n}\nvoid Garage::Service() {\n  car->Start();\n}\n}  // namespace garage\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "garage::Garage::Inspect",
                expected: json!([
                    {"name": "garage::Garage::Inspect", "edges": [
                        {"symbol": {"name": "garage::Engine::Start"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-range-for-receiver.test.ts:213 — after the loop, and in another function, a same-named receiver is not the loop's variable
#[tokio::test]
async fn cpp_range_for_receiver_upstream_213() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_range_for_receiver_upstream_213",
        files: vec![
            SourceFile {
                path: "garage/vehicle.h",
                source: "#include <vector>\nnamespace garage {\nclass Engine {\n public:\n  void Start();\n  int Torque() const;\n};\nclass Car {\n public:\n  void Start();\n};\nnamespace parts {\nclass Part {\n public:\n  void Fit();\n};\nvoid Install();\n}  // namespace parts\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/vehicle.cc",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nvoid Engine::Start() {}\nint Engine::Torque() const { return 0; }\nvoid Car::Start() {}\nvoid parts::Part::Fit() {}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.h",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nclass Garage {\n public:\n  void Inspect(const std::vector<Engine*>& engines);\n  void Service();\n private:\n  Car* car;\n};\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.cc",
                source: "#include \"garage/garage.h\"\nnamespace garage {\nvoid Garage::Inspect(const std::vector<Engine*>& engines) {\n  for (Engine* car : engines) car->Start();\n}\nvoid Garage::Service() {\n  car->Start();\n}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/fleet.cc",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nvoid Drive(Car* engine, const std::vector<Engine*>& engines) {\n  for (Engine* engine : engines) {\n    engine->Start();\n  }\n  engine->Start();\n}\nvoid Wash(Car* engine, const std::vector<Engine*>& engines) {\n  for (Engine* engine : engines) engine->Start();\n  engine->Start();\n}\n}  // namespace garage\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "garage::Drive",
                expected: json!([
                    {"name": "garage::Drive", "edges": [
                        {"symbol": {"name": "garage::Car::Start"}},
                        {"symbol": {"name": "garage::Engine::Start"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "garage::Wash",
                expected: json!([
                    {"name": "garage::Wash", "edges": [
                        {"symbol": {"name": "garage::Car::Start"}},
                        {"symbol": {"name": "garage::Engine::Start"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "garage::Garage::Service",
                expected: json!([
                    {"name": "garage::Garage::Service", "edges": [
                        {"symbol": {"name": "garage::Car::Start"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-range-for-receiver.test.ts:244 — reads references, pointers, qualified and global-scope types, and a header split over lines
#[tokio::test]
async fn cpp_range_for_receiver_upstream_244() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_range_for_receiver_upstream_244",
        files: vec![
            SourceFile {
                path: "garage/vehicle.h",
                source: "#include <vector>\nnamespace garage {\nclass Engine {\n public:\n  void Start();\n  int Torque() const;\n};\nclass Car {\n public:\n  void Start();\n};\nnamespace parts {\nclass Part {\n public:\n  void Fit();\n};\nvoid Install();\n}  // namespace parts\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/vehicle.cc",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nvoid Engine::Start() {}\nint Engine::Torque() const { return 0; }\nvoid Car::Start() {}\nvoid parts::Part::Fit() {}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.h",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nclass Garage {\n public:\n  void Inspect(const std::vector<Engine*>& engines);\n  void Service();\n private:\n  Car* car;\n};\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.cc",
                source: "#include \"garage/garage.h\"\nnamespace garage {\nvoid Garage::Inspect(const std::vector<Engine*>& engines) {\n  for (Engine* car : engines) car->Start();\n}\nvoid Garage::Service() {\n  car->Start();\n}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/shop.cc",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nvoid ByReference(std::vector<Engine>& engines) {\n  for (Engine& e : engines) e.Start();\n}\nvoid ByConstReference(const std::vector<Engine>& engines) {\n  for (const Engine& e : engines) {\n    if (e.Torque() > 0) {}\n  }\n}\nvoid StarOnName(const std::vector<Engine*>& engines) {\n  for (Engine *e : engines)\n    e->Start();\n}\nvoid Qualified(const std::vector<parts::Part*>& list) {\n  for (::garage::parts::Part* p : list) p->Fit();\n}\nvoid SplitHeader(const std::vector<Engine*>& engines) {\n  for (Engine* first_engine_in_the_list :\n       engines) {\n    first_engine_in_the_list->Start();\n  }\n}\n}  // namespace garage\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "garage::ByReference",
                expected: json!([
                    {"name": "garage::ByReference", "edges": [
                        {"symbol": {"name": "garage::Engine::Start"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "garage::ByConstReference",
                expected: json!([
                    {"name": "garage::ByConstReference", "edges": [
                        {"symbol": {"name": "garage::Engine::Torque"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "garage::StarOnName",
                expected: json!([
                    {"name": "garage::StarOnName", "edges": [
                        {"symbol": {"name": "garage::Engine::Start"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "garage::Qualified",
                expected: json!([
                    {"name": "garage::Qualified", "edges": [
                        {"symbol": {"name": "garage::parts::Part::Fit"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "garage::SplitHeader",
                expected: json!([
                    {"name": "garage::SplitHeader", "edges": [
                        {"symbol": {"name": "garage::Engine::Start"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-range-for-receiver.test.ts:286 — a brace in a comment or a string literal does not end the loop
#[tokio::test]
async fn cpp_range_for_receiver_upstream_286() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_range_for_receiver_upstream_286",
        files: vec![
            SourceFile {
                path: "garage/vehicle.h",
                source: "#include <vector>\nnamespace garage {\nclass Engine {\n public:\n  void Start();\n  int Torque() const;\n};\nclass Car {\n public:\n  void Start();\n};\nnamespace parts {\nclass Part {\n public:\n  void Fit();\n};\nvoid Install();\n}  // namespace parts\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/vehicle.cc",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nvoid Engine::Start() {}\nint Engine::Torque() const { return 0; }\nvoid Car::Start() {}\nvoid parts::Part::Fit() {}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.h",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nclass Garage {\n public:\n  void Inspect(const std::vector<Engine*>& engines);\n  void Service();\n private:\n  Car* car;\n};\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.cc",
                source: "#include \"garage/garage.h\"\nnamespace garage {\nvoid Garage::Inspect(const std::vector<Engine*>& engines) {\n  for (Engine* car : engines) car->Start();\n}\nvoid Garage::Service() {\n  car->Start();\n}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/braces.cc",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nvoid Braces(const std::vector<Engine*>& engines) {\n  for (Engine* e : engines) {\n    // A comment's } is not the loop's.\n    const char* close = \"}\";\n    /* nor is } this one\n       } or this */\n    char brace = '}';\n    e->Start();\n  }\n}\n}  // namespace garage\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "garage::Braces",
                expected: json!([
                    {"name": "garage::Braces", "edges": [
                        {"symbol": {"name": "garage::Engine::Start"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-range-for-receiver.test.ts:313 — a nested loop's call is inside the outer loop's body
#[tokio::test]
async fn cpp_range_for_receiver_upstream_313() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_range_for_receiver_upstream_313",
        files: vec![
            SourceFile {
                path: "garage/vehicle.h",
                source: "#include <vector>\nnamespace garage {\nclass Engine {\n public:\n  void Start();\n  int Torque() const;\n};\nclass Car {\n public:\n  void Start();\n};\nnamespace parts {\nclass Part {\n public:\n  void Fit();\n};\nvoid Install();\n}  // namespace parts\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/vehicle.cc",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nvoid Engine::Start() {}\nint Engine::Torque() const { return 0; }\nvoid Car::Start() {}\nvoid parts::Part::Fit() {}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.h",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nclass Garage {\n public:\n  void Inspect(const std::vector<Engine*>& engines);\n  void Service();\n private:\n  Car* car;\n};\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.cc",
                source: "#include \"garage/garage.h\"\nnamespace garage {\nvoid Garage::Inspect(const std::vector<Engine*>& engines) {\n  for (Engine* car : engines) car->Start();\n}\nvoid Garage::Service() {\n  car->Start();\n}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/nested.cc",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nvoid Nested(const std::vector<parts::Part*>& list) {\n  for (parts::Part* p : list) {\n    for (int i = 0; i < 2; ++i) {\n      if (i) { p->Fit(); }\n    }\n  }\n}\n}  // namespace garage\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "garage::Nested",
                expected: json!([
                    {"name": "garage::Nested", "edges": [
                        {"symbol": {"name": "garage::parts::Part::Fit"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-range-for-receiver.test.ts:339 — a `for` in a comment or a string literal
#[tokio::test]
async fn cpp_range_for_receiver_upstream_339() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_range_for_receiver_upstream_339",
        files: vec![
            SourceFile {
                path: "garage/vehicle.h",
                source: "#include <vector>\nnamespace garage {\nclass Engine {\n public:\n  void Start();\n  int Torque() const;\n};\nclass Car {\n public:\n  void Start();\n};\nnamespace parts {\nclass Part {\n public:\n  void Fit();\n};\nvoid Install();\n}  // namespace parts\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/vehicle.cc",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nvoid Engine::Start() {}\nint Engine::Torque() const { return 0; }\nvoid Car::Start() {}\nvoid parts::Part::Fit() {}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.h",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nclass Garage {\n public:\n  void Inspect(const std::vector<Engine*>& engines);\n  void Service();\n private:\n  Car* car;\n};\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.cc",
                source: "#include \"garage/garage.h\"\nnamespace garage {\nvoid Garage::Inspect(const std::vector<Engine*>& engines) {\n  for (Engine* car : engines) car->Start();\n}\nvoid Garage::Service() {\n  car->Start();\n}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/notes.cc",
                source: "#include <string>\n#include \"garage/vehicle.h\"\nnamespace garage {\nvoid Commented(Car* car) {\n  // for (Engine* car : engines) {\n  car->Start();\n}\nvoid Inline(Car* car) {\n  /* for (Engine* car : engines) { */ car->Start();\n}\nstd::string Template(Car* car) {\n  std::string code = \"for (Engine* car : engines) {\";\n  car->Start();\n  return code + \"}\";\n}\n}  // namespace garage\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "garage::Commented",
                expected: json!([
                    {"name": "garage::Commented", "edges": [
                        {"symbol": {"name": "garage::Car::Start"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "garage::Inline",
                expected: json!([
                    {"name": "garage::Inline", "edges": [
                        {"symbol": {"name": "garage::Car::Start"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "garage::Template",
                expected: json!([
                    {"name": "garage::Template", "edges": [
                        {"symbol": {"name": "garage::Car::Start"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-range-for-receiver.test.ts:371 — a bit-field
#[tokio::test]
async fn cpp_range_for_receiver_upstream_371() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_range_for_receiver_upstream_371",
        files: vec![
            SourceFile {
                path: "garage/vehicle.h",
                source: "#include <vector>\nnamespace garage {\nclass Engine {\n public:\n  void Start();\n  int Torque() const;\n};\nclass Car {\n public:\n  void Start();\n};\nnamespace parts {\nclass Part {\n public:\n  void Fit();\n};\nvoid Install();\n}  // namespace parts\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/vehicle.cc",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nvoid Engine::Start() {}\nint Engine::Torque() const { return 0; }\nvoid Car::Start() {}\nvoid parts::Part::Fit() {}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.h",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nclass Garage {\n public:\n  void Inspect(const std::vector<Engine*>& engines);\n  void Service();\n private:\n  Car* car;\n};\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.cc",
                source: "#include \"garage/garage.h\"\nnamespace garage {\nvoid Garage::Inspect(const std::vector<Engine*>& engines) {\n  for (Engine* car : engines) car->Start();\n}\nvoid Garage::Service() {\n  car->Start();\n}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/lot.h",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nstruct Slot {\n  unsigned car : 1;\n};\nclass Lot {\n public:\n  void Park();\n private:\n  Car* car;\n};\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/lot.cc",
                source: "#include \"garage/lot.h\"\nnamespace garage {\nvoid Lot::Park() {\n  car->Start();\n}\n}  // namespace garage\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "garage::Lot::Park",
                expected: json!([
                    {"name": "garage::Lot::Park", "edges": [
                        {"symbol": {"name": "garage::Car::Start"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-range-for-receiver.test.ts:407 — a name followed by the scope operator
#[tokio::test]
async fn cpp_range_for_receiver_upstream_407() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_range_for_receiver_upstream_407",
        files: vec![
            SourceFile {
                path: "garage/vehicle.h",
                source: "#include <vector>\nnamespace garage {\nclass Engine {\n public:\n  void Start();\n  int Torque() const;\n};\nclass Car {\n public:\n  void Start();\n};\nnamespace parts {\nclass Part {\n public:\n  void Fit();\n};\nvoid Install();\n}  // namespace parts\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/vehicle.cc",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nvoid Engine::Start() {}\nint Engine::Torque() const { return 0; }\nvoid Car::Start() {}\nvoid parts::Part::Fit() {}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.h",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nclass Garage {\n public:\n  void Inspect(const std::vector<Engine*>& engines);\n  void Service();\n private:\n  Car* car;\n};\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/garage.cc",
                source: "#include \"garage/garage.h\"\nnamespace garage {\nvoid Garage::Inspect(const std::vector<Engine*>& engines) {\n  for (Engine* car : engines) car->Start();\n}\nvoid Garage::Service() {\n  car->Start();\n}\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/bay.h",
                source: "#include \"garage/vehicle.h\"\nnamespace garage {\nclass Bay {\n public:\n  void Open();\n  void Fit(const std::vector<parts::Part*>& list);\n private:\n  Car* parts;\n};\n}  // namespace garage\n",
            },
            SourceFile {
                path: "garage/bay.cc",
                source: "#include \"garage/bay.h\"\nnamespace garage {\nvoid parts::Install() {}\nvoid Bay::Open() {\n  parts->Start();\n}\nvoid Bay::Fit(const std::vector<parts::Part*>& list) {\n  for (parts::Part* part : list) parts->Start();\n}\n}  // namespace garage\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "garage::Bay::Open",
                expected: json!([
                    {"name": "garage::Bay::Open", "edges": [
                        {"symbol": {"name": "garage::Car::Start"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "garage::Bay::Fit",
                expected: json!([
                    {"name": "garage::Bay::Fit", "edges": [
                        {"symbol": {"name": "garage::Car::Start"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-test-fixture-call.test.ts:121 — reaches the fixture's own and inherited methods
#[tokio::test]
async fn cpp_test_fixture_call_upstream_121() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_test_fixture_call_upstream_121",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual void SeekToFirst() = 0;\n  virtual bool Valid() const = 0;\n};\nclass Iterator : public IteratorBase {};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_test_util.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DBTestBase {\n public:\n  Iterator* NewIterator();\n  void Put(int key);\n  void Flush();\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_test_util.cc",
                source: "#include \"db/db_test_util.h\"\nnamespace rocksdb {\nIterator* DBTestBase::NewIterator() { return nullptr; }\nvoid DBTestBase::Put(int key) {}\nvoid DBTestBase::Flush() {}\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_basic_test.cc",
                source: "#include \"db/db_test_util.h\"\nnamespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    void SeekToFirst() {}\n    bool Valid() const { return false; }\n  };\n  Iterator* NewIterator() { return nullptr; }\n};\nclass WriteBatch {\n public:\n  void Put(int key) {}\n  void Flush() {}\n};\n\nclass DBBasicTest : public DBTestBase {\n public:\n  void Reopen() {}\n};\nTEST_F(DBBasicTest, PutFlush) {\n  Put(1);\n  Flush();\n  Reopen();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::TEST_F",
                expected: json!([
                    {"name": "rocksdb::TEST_F", "edges": [
                        {"symbol": {"name": "rocksdb::DBBasicTest::Reopen"}},
                        {"symbol": {"name": "rocksdb::DBTestBase::Flush"}},
                        {"symbol": {"name": "rocksdb::DBTestBase::Put"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-test-fixture-call.test.ts:151 — types `auto iter = NewIterator()` from the fixture's method
#[tokio::test]
async fn cpp_test_fixture_call_upstream_151() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_test_fixture_call_upstream_151",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual void SeekToFirst() = 0;\n  virtual bool Valid() const = 0;\n};\nclass Iterator : public IteratorBase {};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_test_util.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DBTestBase {\n public:\n  Iterator* NewIterator();\n  void Put(int key);\n  void Flush();\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_test_util.cc",
                source: "#include \"db/db_test_util.h\"\nnamespace rocksdb {\nIterator* DBTestBase::NewIterator() { return nullptr; }\nvoid DBTestBase::Put(int key) {}\nvoid DBTestBase::Flush() {}\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_iterator_test.cc",
                source: "#include \"db/db_test_util.h\"\nnamespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    void SeekToFirst() {}\n    bool Valid() const { return false; }\n  };\n  Iterator* NewIterator() { return nullptr; }\n};\nclass WriteBatch {\n public:\n  void Put(int key) {}\n  void Flush() {}\n};\n\nclass DBIteratorTest : public DBTestBase {};\nTEST_F(DBIteratorTest, IterSeekToFirst) {\n  auto iter = NewIterator();\n  iter->SeekToFirst();\n  if (iter->Valid()) {}\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::TEST_F",
                expected: json!([
                    {"name": "rocksdb::TEST_F", "edges": [
                        {"symbol": {"name": "rocksdb::DBTestBase::NewIterator"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::SeekToFirst"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::Valid"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-test-fixture-call.test.ts:178 — does the same in a TEST_P body, and on `this->`
#[tokio::test]
async fn cpp_test_fixture_call_upstream_178() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_test_fixture_call_upstream_178",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual void SeekToFirst() = 0;\n  virtual bool Valid() const = 0;\n};\nclass Iterator : public IteratorBase {};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_test_util.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DBTestBase {\n public:\n  Iterator* NewIterator();\n  void Put(int key);\n  void Flush();\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_test_util.cc",
                source: "#include \"db/db_test_util.h\"\nnamespace rocksdb {\nIterator* DBTestBase::NewIterator() { return nullptr; }\nvoid DBTestBase::Put(int key) {}\nvoid DBTestBase::Flush() {}\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_param_test.cc",
                source: "#include \"db/db_test_util.h\"\nnamespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    void SeekToFirst() {}\n    bool Valid() const { return false; }\n  };\n  Iterator* NewIterator() { return nullptr; }\n};\nclass WriteBatch {\n public:\n  void Put(int key) {}\n  void Flush() {}\n};\n\nclass DBParamTest : public DBTestBase {};\nTEST_P(DBParamTest, Write) {\n  this->Put(2);\n  Flush();\n  auto it = this->NewIterator();\n  it->SeekToFirst();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::TEST_P",
                expected: json!([
                    {"name": "rocksdb::TEST_P", "edges": [
                        {"symbol": {"name": "rocksdb::DBTestBase::Flush"}},
                        {"symbol": {"name": "rocksdb::DBTestBase::NewIterator"}},
                        {"symbol": {"name": "rocksdb::DBTestBase::Put"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::SeekToFirst"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-test-fixture-call.test.ts:207 — uses the fixture declared in the test's own file, of two sharing its name
#[tokio::test]
async fn cpp_test_fixture_call_upstream_207() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_test_fixture_call_upstream_207",
        files: vec![
            SourceFile {
                path: "include/rocksdb/iterator.h",
                source: "namespace rocksdb {\nclass IteratorBase {\n public:\n  virtual void SeekToFirst() = 0;\n  virtual bool Valid() const = 0;\n};\nclass Iterator : public IteratorBase {};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_test_util.h",
                source: "#include \"include/rocksdb/iterator.h\"\nnamespace rocksdb {\nclass DBTestBase {\n public:\n  Iterator* NewIterator();\n  void Put(int key);\n  void Flush();\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_test_util.cc",
                source: "#include \"db/db_test_util.h\"\nnamespace rocksdb {\nIterator* DBTestBase::NewIterator() { return nullptr; }\nvoid DBTestBase::Put(int key) {}\nvoid DBTestBase::Flush() {}\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/other_test_util.h",
                source: "namespace rocksdb {\nclass MemTableRep {\n public:\n  class Iterator {\n   public:\n    void SeekToFirst() {}\n    bool Valid() const { return false; }\n  };\n  Iterator* NewIterator() { return nullptr; }\n};\nclass WriteBatch {\n public:\n  void Put(int key) {}\n  void Flush() {}\n};\n\nclass OtherTestBase {\n public:\n  MemTableRep::Iterator* NewIterator() { return nullptr; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/a_test.cc",
                source: "#include \"db/other_test_util.h\"\nnamespace rocksdb {\nclass SharedTest : public OtherTestBase {};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/b_test.cc",
                source: "#include \"db/db_test_util.h\"\nnamespace rocksdb {\nclass SharedTest : public DBTestBase {};\nTEST_F(SharedTest, Seek) {\n  auto iter = NewIterator();\n  iter->SeekToFirst();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::TEST_F",
                expected: json!([
                    {"name": "rocksdb::TEST_F", "edges": [
                        {"symbol": {"name": "rocksdb::DBTestBase::NewIterator"}},
                        {"symbol": {"name": "rocksdb::IteratorBase::SeekToFirst"}},
                    ]},
                ]),
                file: Some("db/b_test.cc"),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-test-fixture-call.test.ts:246 — types a method's bare call on what the inherited method returns, past a nested class's parameter of its name
#[tokio::test]
async fn cpp_test_fixture_call_upstream_246() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_test_fixture_call_upstream_246",
        files: vec![
            SourceFile {
                path: "db/db_test.cc",
                source: "namespace leveldb {\nclass Env {\n public:\n  virtual int NewRandomAccessFile() { return 0; }\n};\nclass EnvWrapper : public Env {\n public:\n  Env* target() const { return target_; }\n private:\n  Env* target_;\n};\nclass SpecialEnv : public EnvWrapper {\n public:\n  int NewRandomAccessFile() override {\n    class CountingFile {\n     public:\n      explicit CountingFile(Env* target) : target_(target) {}\n     private:\n      Env* target_;\n    };\n    return target()->NewRandomAccessFile();\n  }\n};\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::SpecialEnv::NewRandomAccessFile",
                expected: json!([
                    {"name": "leveldb::SpecialEnv::NewRandomAccessFile", "edges": [
                        {"symbol": {"name": "leveldb::Env::NewRandomAccessFile"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-test-fixture-call.test.ts:284 — types a template's bare call into a base it names with a using-declaration
#[tokio::test]
async fn cpp_test_fixture_call_upstream_284() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_test_fixture_call_upstream_284",
        files: vec![
            SourceFile {
                path: "src/google/protobuf/reflection_internal.h",
                source: "namespace google {\nnamespace protobuf {\ntemplate <typename T>\nclass RepeatedField {\n public:\n  void Swap(RepeatedField* other) {}\n};\ntemplate <typename T>\nclass RepeatedFieldWrapper {\n public:\n  RepeatedField<T>* MutableRepeatedField(void* data) const { return nullptr; }\n};\ntemplate <typename T>\nclass RepeatedFieldPrimitiveAccessor final : public RepeatedFieldWrapper<T> {\n  using RepeatedFieldWrapper<T>::MutableRepeatedField;\n public:\n  void Swap(void* data, void* other_data) const {\n    MutableRepeatedField(data)->Swap(MutableRepeatedField(other_data));\n  }\n};\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::RepeatedFieldPrimitiveAccessor::Swap",
                expected: json!([
                    {"name": "google::protobuf::RepeatedFieldPrimitiveAccessor::Swap", "edges": [
                        {"symbol": {"name": "google::protobuf::RepeatedField::Swap"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-translation-unit-visibility.test.ts:59 — is not another source file's class in an unnamed namespace
#[tokio::test]
async fn cpp_translation_unit_visibility_upstream_59() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_translation_unit_visibility_upstream_59",
        files: vec![
            SourceFile {
                path: "memtable/hash_linklist_rep.cc",
                source: "namespace rocksdb {\nnamespace {\nstruct Node {\n  Node* Next() { return nullptr; }\n};\n}  // namespace\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "memtable/inlineskiplist.h",
                source: "namespace rocksdb {\ntemplate <class Comparator>\nclass InlineSkipList {\n private:\n  struct Node;\n public:\n  void Insert() {\n    Node* x = head_;\n    x->Next();\n  }\n private:\n  Node* head_;\n};\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::InlineSkipList::Insert",
                expected: json!([
                    {"name": "rocksdb::InlineSkipList::Insert", "edges": [
                    ]},
                ]),
                edge_filters: vec![("includes", "hash_linklist_rep.cc", false)],
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-translation-unit-visibility.test.ts:94 — is not another test file's fixture method a bare call names
#[tokio::test]
async fn cpp_translation_unit_visibility_upstream_94() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_translation_unit_visibility_upstream_94",
        files: vec![
            SourceFile {
                path: "db/column_family_test.cc",
                source: "namespace rocksdb {\nclass ColumnFamilyTestBase {\n public:\n  void Put(int key) {}\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/db_basic_test.cc",
                source: "namespace rocksdb {\nvoid Put(int key);\nvoid Check() {\n  Put(1);\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Check",
                expected: json!([
                    {"name": "rocksdb::Check", "edges": [
                    ]},
                ]),
                edge_filters: vec![("includes", "column_family_test.cc", false)],
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-translation-unit-visibility.test.ts:120 — is not a function another source file's unnamed namespace declares
#[tokio::test]
async fn cpp_translation_unit_visibility_upstream_120() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_translation_unit_visibility_upstream_120",
        files: vec![
            SourceFile {
                path: "conformance/matchers.cc",
                source: "namespace conformance {\nnamespace {\nint RawPayload(int response) { return response; }\n}  // namespace\n}  // namespace conformance\n",
            },
            SourceFile {
                path: "conformance/matchers_test.cc",
                source: "namespace conformance {\nint Describe() {\n  return RawPayload(1);\n}\n}  // namespace conformance\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "conformance::Describe",
                expected: json!([
                    {"name": "conformance::Describe", "edges": [
                    ]},
                ]),
                edge_filters: vec![("includes", "matchers.cc", false)],
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-translation-unit-visibility.test.ts:144 — is a member of the header's class, not of a source file's namesake
#[tokio::test]
async fn cpp_translation_unit_visibility_upstream_144() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_translation_unit_visibility_upstream_144",
        files: vec![
            SourceFile {
                path: "cache/cache_bench_tool.cc",
                source: "namespace rocksdb {\nclass SharedState {\n public:\n  int* GetMutex() { return nullptr; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db_stress_tool/db_stress_shared_state.h",
                source: "namespace rocksdb {\nclass SharedState {\n public:\n  int* GetMutex() { return nullptr; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db_stress_tool/db_stress_common.cc",
                source: "#include \"db_stress_tool/db_stress_shared_state.h\"\nnamespace rocksdb {\nvoid Lock(SharedState* shared) {\n  shared->GetMutex();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Lock",
                expected: json!([
                    {"name": "rocksdb::Lock", "edges": [
                        {"symbol": {"name": "rocksdb::SharedState::GetMutex", "filePath": "db_stress_tool/db_stress_shared_state.h"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-translation-unit-visibility.test.ts:180 — is the header's class for a receiver a namespace-scope variable holds
#[tokio::test]
async fn cpp_translation_unit_visibility_upstream_180() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_translation_unit_visibility_upstream_180",
        files: vec![
            SourceFile {
                path: "cache/cache_bench_tool.cc",
                source: "namespace rocksdb {\nclass SharedState {\n public:\n  int* GetMutex() { return nullptr; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db_stress_tool/db_stress_shared_state.h",
                source: "namespace rocksdb {\nclass SharedState {\n public:\n  int* GetMutex() { return nullptr; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db_stress_tool/db_stress_common.cc",
                source: "#include \"db_stress_tool/db_stress_shared_state.h\"\nnamespace rocksdb {\nSharedState* shared_state = nullptr;\nvoid LockShared() {\n  shared_state->GetMutex();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::LockShared",
                expected: json!([
                    {"name": "rocksdb::LockShared", "edges": [
                        {"symbol": {"name": "rocksdb::SharedState::GetMutex", "filePath": "db_stress_tool/db_stress_shared_state.h"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-translation-unit-visibility.test.ts:215 — is a header's class, with no include the index follows
#[tokio::test]
async fn cpp_translation_unit_visibility_upstream_215() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_translation_unit_visibility_upstream_215",
        files: vec![
            SourceFile {
                path: "include/rocksdb/cache.h",
                source: "namespace rocksdb {\nclass Cache {\n public:\n  virtual int Insert(int key) { return key; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "cache/user.cc",
                source: "namespace rocksdb {\nint Use(Cache* cache) {\n  return cache->Insert(1);\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Use",
                expected: json!([
                    {"name": "rocksdb::Use", "edges": [
                        {"symbol": {"name": "rocksdb::Cache::Insert", "filePath": "include/rocksdb/cache.h"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-translation-unit-visibility.test.ts:240 — is not an out-of-line method of a class another source file declares
#[tokio::test]
async fn cpp_translation_unit_visibility_upstream_240() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_translation_unit_visibility_upstream_240",
        files: vec![
            SourceFile {
                path: "src/descriptor.cc",
                source: "namespace google {\nnamespace protobuf {\nclass FileDescriptorTables {\n public:\n  const int* FindFieldByLowercaseName(const char* name) const;\n};\nconst int* FileDescriptorTables::FindFieldByLowercaseName(const char* name) const {\n  return nullptr;\n}\n}  // namespace protobuf\n}  // namespace google\n",
            },
            SourceFile {
                path: "python/message.cc",
                source: "namespace google {\nnamespace protobuf {\nnamespace python {\nconst int* Find(CMessage* self, const char* name) {\n  return self->message_descriptor->FindFieldByLowercaseName(name);\n}\n}  // namespace python\n}  // namespace protobuf\n}  // namespace google\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::python::Find",
                expected: json!([
                    {"name": "google::protobuf::python::Find", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-translation-unit-visibility.test.ts:306 — reads an unnamed namespace past comments, preprocessor lines and literals that spell one
#[tokio::test]
async fn cpp_translation_unit_visibility_upstream_306() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_translation_unit_visibility_upstream_306",
        files: vec![
            SourceFile {
                path: "db/c.cc",
                source: "namespace rocksdb {\n// namespace { in a line comment\n/* namespace { in a block comment */\n#define ROCKSDB_ANONYMOUS_BEGIN namespace {\nconst char* kDoc = \"namespace {\";\nconst char* kRaw = R\"(a \"b namespace {\" c)\";\nint Helper(int x) { return x; }\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/crlf.cc",
                source: "namespace rocksdb {\r\n#define ROCKSDB_ANONYMOUS_BEGIN \\\r\n  namespace {\r\nint Other(int x) { return x; }\r\n}  // namespace rocksdb\r\n",
            },
            SourceFile {
                path: "db/c_test.cc",
                source: "namespace rocksdb {\nint Check() {\n  return Helper(1) + Other(2);\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Check",
                expected: json!([
                    {"name": "rocksdb::Check", "edges": [
                        {"symbol": {"name": "rocksdb::Helper", "filePath": "db/c.cc"}},
                        {"symbol": {"name": "rocksdb::Other", "filePath": "db/crlf.cc"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-translation-unit-visibility.test.ts:335 — is still the file's own, and a header's it includes after the declaration
#[tokio::test]
async fn cpp_translation_unit_visibility_upstream_335() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_translation_unit_visibility_upstream_335",
        files: vec![
            SourceFile {
                path: "db/version_set.cc",
                source: "namespace rocksdb {\nnamespace {\nclass FilePicker {\n public:\n  int GetNextFile() { return 0; }\n};\n}  // namespace\nint Pick(FilePicker& fp) {\n  return fp.GetNextFile();\n}\n}  // namespace rocksdb\n#include \"db/version_set_sync_and_async.h\"\n",
            },
            SourceFile {
                path: "db/version_set_sync_and_async.h",
                source: "namespace rocksdb {\nint PickAsync(FilePicker& fp) {\n  return fp.GetNextFile();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::Pick",
                expected: json!([
                    {"name": "rocksdb::Pick", "edges": [
                        {"symbol": {"name": "rocksdb::FilePicker::GetNextFile", "filePath": "db/version_set.cc"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::PickAsync",
                expected: json!([
                    {"name": "rocksdb::PickAsync", "edges": [
                        {"symbol": {"name": "rocksdb::FilePicker::GetNextFile", "filePath": "db/version_set.cc"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-translation-unit-visibility.test.ts:367 — is seen by a file that includes the source file
#[tokio::test]
async fn cpp_translation_unit_visibility_upstream_367() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_translation_unit_visibility_upstream_367",
        files: vec![
            SourceFile {
                path: "util/impl.cc",
                source: "namespace {\nclass Impl {\n public:\n  int Run() { return 0; }\n};\n}  // namespace\n",
            },
            SourceFile {
                path: "util/impl_test.cc",
                source: "#include \"util/impl.cc\"\nint Test(Impl& impl) {\n  return impl.Run();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Test",
                expected: json!([
                    {"name": "Test", "edges": [
                        {"symbol": {"name": "Impl::Run", "filePath": "util/impl.cc"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-translation-unit-visibility.test.ts:391 — keeps an out-of-line method of a class a header declares, through an alias too
#[tokio::test]
async fn cpp_translation_unit_visibility_upstream_391() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_translation_unit_visibility_upstream_391",
        files: vec![
            SourceFile {
                path: "include/rocksdb/multi_scan.h",
                source: "namespace rocksdb {\nclass MultiScan {\n public:\n  class MultiScanIterator {\n   public:\n    void SeekCurrentRange();\n    void Next() { SeekCurrentRange(); }\n  };\n};\nclass Table {\n public:\n  int Open();\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/multi_scan.cc",
                source: "#include \"include/rocksdb/multi_scan.h\"\nnamespace rocksdb {\nusing MultiScanIterator = MultiScan::MultiScanIterator;\nvoid MultiScanIterator::SeekCurrentRange() {}\nint Table::Open() { return 0; }\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/open.cc",
                source: "#include \"include/rocksdb/multi_scan.h\"\nnamespace rocksdb {\nint OpenTable(Table* t) {\n  return t->Open();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::MultiScan::MultiScanIterator::Next",
                expected: json!([
                    {"name": "rocksdb::MultiScan::MultiScanIterator::Next", "edges": [
                        {},
                    ]},
                ]),
                edge_filters: vec![("includes", "SeekCurrentRange", false)],
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "rocksdb::OpenTable",
                expected: json!([
                    {"name": "rocksdb::OpenTable", "edges": [
                        {"symbol": {"name": "rocksdb::Table::Open", "filePath": "db/multi_scan.cc"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-translation-unit-visibility.test.ts:434 — keeps a namespace's function a source file defines out of line, beside another file's namesake struct
#[tokio::test]
async fn cpp_translation_unit_visibility_upstream_434() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_translation_unit_visibility_upstream_434",
        files: vec![
            SourceFile {
                path: "test/posix-mock.h",
                source: "namespace test {\nint close(int fildes);\n}  // namespace test\n",
            },
            SourceFile {
                path: "test/posix-mock-test.cc",
                source: "#include \"posix-mock.h\"\nint test::close(int fildes) { return fildes; }\n",
            },
            SourceFile {
                path: "test/ostream-test.cc",
                source: "struct test {};\n",
            },
            SourceFile {
                path: "test/os-test.cc",
                source: "#include \"posix-mock.h\"\nint Shut(int fd) {\n  return test::close(fd);\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Shut",
                expected: json!([
                    {"name": "Shut", "edges": [
                        {},
                    ]},
                ]),
                edge_filters: vec![("startsWith", "test::close", false)],
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-type-alias-receiver.test.ts:156 — reproduction: `table_.Insert` reaches SkipList::Insert, not a class named after the receiver
// cpp-type-alias-receiver.test.ts:166 — a nested type reached through the alias is constructed and called on the aliased type
// cpp-type-alias-receiver.test.ts:178 — a local object of the alias type constructs the aliased type, never an outer class the alias hides
// cpp-type-alias-receiver.test.ts:191 — follows an alias of an alias
#[tokio::test]
async fn cpp_type_alias_receiver_upstream_156() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_type_alias_receiver_upstream_156",
        files: vec![
            SourceFile {
                path: "include/leveldb/table.h",
                source: "namespace leveldb {\nclass Table {\n public:\n  explicit Table(int* rep);\n  int ApproximateOffsetOf(int key) const;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/cache.cc",
                source: "namespace leveldb {\nclass HandleTable {\n public:\n  void Insert(int h) { last_ = h; }\n private:\n  int last_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/skiplist.h",
                source: "namespace leveldb {\ntemplate <typename Key, class Comparator>\nclass SkipList {\n public:\n  explicit SkipList(Comparator cmp, int* arena);\n  void Insert(const Key& key);\n  class Iterator {\n   public:\n    explicit Iterator(const SkipList* list);\n    void Seek(const Key& target);\n  };\n};\ntemplate <typename Key, class Comparator>\nSkipList<Key, Comparator>::SkipList(Comparator cmp, int* arena) {}\ntemplate <typename Key, class Comparator>\nvoid SkipList<Key, Comparator>::Insert(const Key& key) {}\ntemplate <typename Key, class Comparator>\nSkipList<Key, Comparator>::Iterator::Iterator(const SkipList* list) {}\ntemplate <typename Key, class Comparator>\nvoid SkipList<Key, Comparator>::Iterator::Seek(const Key& target) {}\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/memtable.h",
                source: "#include \"db/skiplist.h\"\nnamespace leveldb {\nstruct KeyComparator {\n  int operator()(const char* a, const char* b) const;\n};\nclass MemTable {\n public:\n  void Add(const char* buf);\n  bool Get(const char* key);\n  void Rebuild(int* arena);\n  void Index(const char* key);\n private:\n  typedef SkipList<const char*, KeyComparator> Table;\n  using Lookup = Table;\n  Table table_;\n  Lookup lookup_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/memtable.cc",
                source: "#include \"db/memtable.h\"\nnamespace leveldb {\nvoid MemTable::Add(const char* buf) {\n  table_.Insert(buf);\n}\nbool MemTable::Get(const char* key) {\n  Table::Iterator iter(&table_);\n  iter.Seek(key);\n  return true;\n}\nvoid MemTable::Rebuild(int* arena) {\n  KeyComparator cmp;\n  Table fresh(cmp, &arena[0]);\n  Table stale(&arena[1]);\n}\nvoid MemTable::Index(const char* key) {\n  lookup_.Insert(key);\n}\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::MemTable::Add",
                expected: json!([
                    {"name": "leveldb::MemTable::Add", "edges": [
                        {"symbol": {"name": "leveldb::SkipList::Insert", "filePath": "db/skiplist.h"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "leveldb::HandleTable::Insert",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "leveldb::MemTable::Get",
                expected: json!([
                    {"name": "leveldb::MemTable::Get", "edges": [
                        {"symbol": {"name": "leveldb::SkipList::Iterator::Iterator", "filePath": "db/skiplist.h"}},
                        {"symbol": {"name": "leveldb::SkipList::Iterator::Seek", "filePath": "db/skiplist.h"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "leveldb::MemTable::Rebuild",
                expected: json!([
                    {"name": "leveldb::MemTable::Rebuild", "edges": [
                        {"symbol": {"name": "leveldb::SkipList::SkipList", "filePath": "db/skiplist.h"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "leveldb::Table::Table",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "leveldb::MemTable::Index",
                expected: json!([
                    {"name": "leveldb::MemTable::Index", "edges": [
                        {"symbol": {"name": "leveldb::SkipList::Insert", "filePath": "db/skiplist.h"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-type-alias-receiver.test.ts:200 — an alias spelled from outside its class, and one declared in the calling function
#[tokio::test]
async fn cpp_type_alias_receiver_upstream_200() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_type_alias_receiver_upstream_200",
        files: vec![
            SourceFile {
                path: "include/leveldb/table.h",
                source: "namespace leveldb {\nclass Table {\n public:\n  explicit Table(int* rep);\n  int ApproximateOffsetOf(int key) const;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "util/cache.cc",
                source: "namespace leveldb {\nclass HandleTable {\n public:\n  void Insert(int h) { last_ = h; }\n private:\n  int last_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/skiplist.h",
                source: "namespace leveldb {\ntemplate <typename Key, class Comparator>\nclass SkipList {\n public:\n  explicit SkipList(Comparator cmp, int* arena);\n  void Insert(const Key& key);\n  class Iterator {\n   public:\n    explicit Iterator(const SkipList* list);\n    void Seek(const Key& target);\n  };\n};\ntemplate <typename Key, class Comparator>\nSkipList<Key, Comparator>::SkipList(Comparator cmp, int* arena) {}\ntemplate <typename Key, class Comparator>\nvoid SkipList<Key, Comparator>::Insert(const Key& key) {}\ntemplate <typename Key, class Comparator>\nSkipList<Key, Comparator>::Iterator::Iterator(const SkipList* list) {}\ntemplate <typename Key, class Comparator>\nvoid SkipList<Key, Comparator>::Iterator::Seek(const Key& target) {}\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/memtable.h",
                source: "#include \"db/skiplist.h\"\nnamespace leveldb {\nstruct KeyComparator {\n  int operator()(const char* a, const char* b) const;\n};\nclass MemTable {\n public:\n  void Add(const char* buf);\n  bool Get(const char* key);\n  void Rebuild(int* arena);\n  void Index(const char* key);\n private:\n  typedef SkipList<const char*, KeyComparator> Table;\n  using Lookup = Table;\n  Table table_;\n  Lookup lookup_;\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/memtable.cc",
                source: "#include \"db/memtable.h\"\nnamespace leveldb {\nvoid MemTable::Add(const char* buf) {\n  table_.Insert(buf);\n}\nbool MemTable::Get(const char* key) {\n  Table::Iterator iter(&table_);\n  iter.Seek(key);\n  return true;\n}\nvoid MemTable::Rebuild(int* arena) {\n  KeyComparator cmp;\n  Table fresh(cmp, &arena[0]);\n  Table stale(&arena[1]);\n}\nvoid MemTable::Index(const char* key) {\n  lookup_.Insert(key);\n}\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/builder.cc",
                source: "#include \"db/memtable.h\"\nnamespace leveldb {\nvoid Fill(MemTable::Table* t) {\n  t->Insert(\"k\");\n}\nvoid Build(int* arena) {\n  typedef SkipList<const char*, KeyComparator> List;\n  KeyComparator cmp;\n  List list(cmp, &arena[0]);\n  list.Insert(\"k\");\n}\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::Fill",
                expected: json!([
                    {"name": "leveldb::Fill", "edges": [
                        {"symbol": {"name": "leveldb::SkipList::Insert", "filePath": "db/skiplist.h"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "leveldb::Build",
                expected: json!([
                    {"name": "leveldb::Build", "edges": [
                        {"symbol": {"name": "leveldb::SkipList::Insert", "filePath": "db/skiplist.h"}},
                        {"symbol": {"name": "leveldb::SkipList::SkipList", "filePath": "db/skiplist.h"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-type-alias-receiver.test.ts:309 — a class declared in an inner scope hides an outer alias of the same name
// outside it the alias applies
#[tokio::test]
async fn cpp_type_alias_receiver_upstream_309() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_type_alias_receiver_upstream_309",
        files: vec![
            SourceFile {
                path: "app/holder.h",
                source: "namespace app {\nclass Gadget {\n public:\n  void Spin();\n};\nusing Widget = Gadget;\nclass Holder {\n public:\n  class Widget {\n   public:\n    void Spin();\n  };\n  void Use();\n private:\n  Widget w_;\n};\n}  // namespace app\n",
            },
            SourceFile {
                path: "app/holder.cc",
                source: "#include \"app/holder.h\"\nnamespace app {\nvoid Gadget::Spin() {}\nvoid Holder::Widget::Spin() {}\nvoid Holder::Use() {\n  w_.Spin();\n}\nvoid Twirl(Widget& w) {\n  w.Spin();\n}\n}  // namespace app\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "app::Holder::Use",
                expected: json!([
                    {"name": "app::Holder::Use", "edges": [
                        {"symbol": {"name": "app::Holder::Widget::Spin", "filePath": "app/holder.cc"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "app::Twirl",
                expected: json!([
                    {"name": "app::Twirl", "edges": [
                        {"symbol": {"name": "app::Gadget::Spin", "filePath": "app/holder.cc"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-type-alias-receiver.test.ts:357 — is found in the base class before an enclosing namespace, and a dependent alias is not followed
#[tokio::test]
async fn cpp_type_alias_receiver_upstream_357() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_type_alias_receiver_upstream_357",
        files: vec![
            SourceFile {
                path: "protobuf/descriptor.h",
                source: "namespace google { namespace protobuf {\nclass FieldDescriptor {\n public:\n  bool is_repeated() const;\n};\ninline bool FieldDescriptor::is_repeated() const { return false; }\nclass OneofDescriptor {\n public:\n  bool is_repeated() const { return false; }\n};\n} }\n",
            },
            SourceFile {
                path: "protobuf/json/traits.h",
                source: "#include \"protobuf/descriptor.h\"\nnamespace google { namespace protobuf { namespace json_internal {\ntemplate <typename Traits>\nusing Field = typename Traits::Field;\nstruct Proto2Descriptor {\n  using Field = const FieldDescriptor*;\n};\nstruct ParseProto2Descriptor : Proto2Descriptor {\n  static bool NewMsg(Field f) { return f->is_repeated(); }\n};\n} } }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::json_internal::ParseProto2Descriptor::NewMsg",
                expected: json!([
                    {"name": "google::protobuf::json_internal::ParseProto2Descriptor::NewMsg", "edges": [
                        {"symbol": {"name": "google::protobuf::FieldDescriptor::is_repeated", "filePath": "protobuf/descriptor.h"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-type-alias-receiver.test.ts:401 — names no type, so a class that shares the alias's name is not it
#[tokio::test]
async fn cpp_type_alias_receiver_upstream_401() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_type_alias_receiver_upstream_401",
        files: vec![
            SourceFile {
                path: "protobuf/type.pb.h",
                source: "namespace google { namespace protobuf {\nclass Type {\n public:\n  void Clear();\n};\ninline void Type::Clear() {}\n} }\n",
            },
            SourceFile {
                path: "protobuf/repeated_ptr_field.h",
                source: "namespace google { namespace protobuf { namespace internal {\ntemplate <typename GenericType>\nclass GenericTypeHandler {\n public:\n  using Type = GenericType;\n  static void Reset(Type* value) { value->Clear(); }\n};\n} } }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "google::protobuf::internal::GenericTypeHandler::Reset",
                expected: json!([
                    {"name": "google::protobuf::internal::GenericTypeHandler::Reset", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-type-alias-receiver.test.ts:437 — `->` through an alias of an iterator reaches the element type, which the alias does not name
#[tokio::test]
async fn cpp_type_alias_receiver_upstream_437() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_type_alias_receiver_upstream_437",
        files: vec![
            SourceFile {
                path: "gmock/expectation.h",
                source: "#include <set>\nnamespace testing {\nclass Expectation {\n public:\n  int expectation_base() const { return 0; }\n};\nclass ExpectationSet {\n public:\n  typedef ::std::set<Expectation> Set;\n  typedef Set::const_iterator const_iterator;\n};\nint Describe(const ExpectationSet& set) {\n  ExpectationSet::const_iterator it;\n  return it->expectation_base();\n}\n}  // namespace testing\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "testing::Describe",
                expected: json!([
                    {"name": "testing::Describe", "edges": [
                        {"symbol": {"name": "testing::Expectation::expectation_base", "filePath": "gmock/expectation.h"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-type-alias-receiver.test.ts:511 — a declaration read from another class is not known to be the receiver's
#[tokio::test]
async fn cpp_type_alias_receiver_upstream_511() {
    // Checks call targets; upstream confidence scores and unrelated extraction assertions are not exposed by zg.
    check_case(Case {
        name: "cpp_type_alias_receiver_upstream_511",
        files: vec![
            SourceFile {
                path: "test/scan.h",
                source: "namespace fmt {\nclass basic_scan_arg {\n public:\n  int type() const { return 0; }\n};\nusing scan_arg = basic_scan_arg;\nclass scan_args {\n  const scan_arg* data;\n};\ntemplate <typename... T> class scan_data {\n public:\n  int make_args() { return 0; }\n};\ntemplate <typename... T> int scan() {\n  auto data = scan_data<T...>();\n  return data.make_args();\n}\n}  // namespace fmt\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "fmt::scan",
                expected: json!([
                    {"name": "fmt::scan", "edges": [
                        {"symbol": {"name": "fmt::scan_data::make_args", "filePath": "test/scan.h"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// namespace-object-resolution.test.ts:31 — resolves Api.member() to the function the shorthand property names
#[tokio::test]
async fn namespace_object_resolution_31() {
    check_case(Case {
        name: "namespace_object_resolution_31",
        files: vec![
            SourceFile {
                path: "package.json",
                source: "{\"name\":\"app\"}",
            },
            SourceFile {
                path: "src/api/frames.ts",
                source: "export async function uploadARCapture(uri: string) {\n  return uri\n}\n",
            },
            SourceFile {
                path: "src/api/folders.ts",
                source: "export function createFolder(name: string) {\n  return name\n}\n",
            },
            SourceFile {
                path: "src/api/index.ts",
                source: "import { uploadARCapture } from './frames'\nimport { createFolder } from './folders'\nfunction localHelper() {\n  return 1\n}\nconst UploadApi = {\n  uploadARCapture,\n  makeFolder: createFolder,\n  localHelper,\n}\nexport default UploadApi\n",
            },
            SourceFile {
                path: "src/hooks.ts",
                source: "import UploadApi from './api'\nexport function handleZipComplete(uri: string) {\n  UploadApi.makeFolder(uri)\n  UploadApi.localHelper()\n  return UploadApi.uploadARCapture(uri)\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "handleZipComplete",
                expected: json!([
                    {"name": "handleZipComplete", "edges": [
                        {"symbol": {"name": "createFolder"}},
                        {"symbol": {"name": "localHelper"}},
                        {"symbol": {"name": "uploadARCapture"}},
                    ]},
                ]),
                short_names: true,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// namespace-object-resolution.test.ts:61 — a default import of a later-exported const finds that const, and a method inside it
#[tokio::test]
async fn namespace_object_resolution_61() {
    check_case(Case {
        name: "namespace_object_resolution_61",
        files: vec![
            SourceFile {
                path: "package.json",
                source: "{\"name\":\"app\"}",
            },
            SourceFile {
                path: "src/store.ts",
                source: "const useStore = {\n  read() {\n    return 1\n  },\n}\nexport function unrelated() {\n  return 2\n}\nexport default useStore\n",
            },
            SourceFile {
                path: "src/use.ts",
                source: "import store from './store'\nexport function consume() {\n  return store.read()\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "consume",
                expected: json!([
                    {"name": "consume", "edges": [
                        {"symbol": {"name": "read"}},
                    ]},
                ]),
                short_names: true,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// ts-this-field-call.test.ts:98 — resolves on the field's declared type even when the wrapper shares the method name
// ts-this-field-call.test.ts:106 — reads a JS field initialized in the constructor
// ts-this-field-call.test.ts:110 — leaves a builtin-typed field unresolved rather than guessing a same-named method
// ts-this-field-call.test.ts:116 — resolves a field typed `typeof <objectLiteral>` onto the literal's member
// ts-this-field-call.test.ts:125 — resolves an ES private field on its declared or constructed type (#1987)
// ts-this-field-call.test.ts:130 — leaves a builtin-typed ES private field unresolved (#1987)
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn ts_this_field_call_98() {
    check_case(Case {
        name: "ts_this_field_call_98",
        files: vec![
            SourceFile {
                path: "src/mailer.ts",
                source: "export class Mailer {\n  send(msg: string): string { return msg; }\n}\n",
            },
            SourceFile {
                path: "src/notifier.ts",
                source: "import { Mailer } from './mailer';\nexport class Notifier {\n  constructor(private readonly mailer: Mailer, private items: string[]) {}\n  send(msg: string): string { return this.mailer.send(msg); }\n  other(msg: string): string { return this.mailer.send(msg); }\n  push(msg: string): void { this.items.push(msg); }\n}\n",
            },
            SourceFile {
                path: "src/legacy-mailer.js",
                source: "class LegacyMailer {\n  send(msg) { return msg; }\n}\nmodule.exports = { LegacyMailer };\n",
            },
            SourceFile {
                path: "src/legacy.js",
                source: "const { LegacyMailer } = require('./legacy-mailer');\nclass LegacyNotifier {\n  constructor() { this.mailer = new LegacyMailer(); }\n  send(msg) { return this.mailer.send(msg); }\n}\nmodule.exports = { LegacyNotifier };\n",
            },
            SourceFile {
                path: "src/storage.ts",
                source: "export const DraftHubStorage = {\n  async get(key: string): Promise<string> { return key; },\n  async getSettings(): Promise<object> { return {}; },\n};\n",
            },
            SourceFile {
                path: "src/keeper.ts",
                source: "import { DraftHubStorage } from './storage';\nexport class Keeper {\n  constructor(private readonly storage: typeof DraftHubStorage) {}\n  async get(key: string): Promise<string> { return this.storage.get(key); }\n  async settings(): Promise<object> { return this.storage.getSettings(); }\n}\n",
            },
            SourceFile {
                path: "src/vault.ts",
                source: "import { Mailer } from './mailer';\nexport class Outbox {\n  send(msg: string): string { return msg; }\n}\nexport class Cart {\n  add(item: string): void {}\n}\nexport class Vault {\n  #mailer: Mailer;\n  #backup = new Mailer();\n  #items = new Set<string>();\n  constructor(m: Mailer) { this.#mailer = m; }\n  notify(msg: string): string { return this.#mailer.send(msg); }\n  fallback(msg: string): string { return this.#backup.send(msg); }\n  put(x: string): void { this.#items.add(x); }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Notifier::send",
                expected: json!([
                    {"name": "Notifier::send", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Notifier::other",
                expected: json!([
                    {"name": "Notifier::other", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Notifier::send",
                expected: json!([
                    {"name": "Notifier::send", "edges": [
                        {"symbol": {"name": "Notifier::send"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "LegacyNotifier::send",
                expected: json!([
                    {"name": "LegacyNotifier::send", "edges": [
                        {"symbol": {"name": "LegacyMailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Notifier::push",
                expected: json!([
                    {"name": "Notifier::push", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Keeper::settings",
                expected: json!([
                    {"name": "Keeper::settings", "edges": [
                        {"symbol": {"name": "DraftHubStorage::getSettings"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Keeper::get",
                expected: json!([
                    {"name": "Keeper::get", "edges": [
                        {"symbol": {"name": "DraftHubStorage::get"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Keeper::get",
                expected: json!([
                    {"name": "Keeper::get", "edges": [
                        {"symbol": {"name": "Keeper::get"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::notify",
                expected: json!([
                    {"name": "Vault::notify", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::fallback",
                expected: json!([
                    {"name": "Vault::fallback", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::put",
                expected: json!([
                    {"name": "Vault::put", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// ts-this-field-call.test.ts:145 — keeps optional receivers distinct from public fields (ts, LF)
#[tokio::test]
async fn ts_this_field_call_146_ts_lf() {
    check_case(Case {
        name: "ts_this_field_call_146_ts_lf",
        files: vec![
            SourceFile {
                path: "vault.ts",
                source: "\nexport class Mailer { send() {} }\nexport class Cart { add() {} }\nexport class Vault {\n  #mailer = new Mailer();\n  #items = new Set();\n  items = new Cart();\n  notify() { this.#mailer?.send(); }\n  optional() { this.#mailer.send?.(); }\n  put() { this.#items?.add('x'); }\n  publicPut() { this.items.add('x'); }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Vault::notify",
                expected: json!([
                    {"name": "Vault::notify", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::optional",
                expected: json!([
                    {"name": "Vault::optional", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::put",
                expected: json!([
                    {"name": "Vault::put", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::publicPut",
                expected: json!([
                    {"name": "Vault::publicPut", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// ts-this-field-call.test.ts:145 — keeps optional receivers distinct from public fields (ts, CRLF)
#[tokio::test]
async fn ts_this_field_call_146_ts_crlf() {
    check_case(Case {
        name: "ts_this_field_call_146_ts_crlf",
        files: vec![
            SourceFile {
                path: "vault.ts",
                source: "\r\nexport class Mailer { send() {} }\r\nexport class Cart { add() {} }\r\nexport class Vault {\r\n  #mailer = new Mailer();\r\n  #items = new Set();\r\n  items = new Cart();\r\n  notify() { this.#mailer?.send(); }\r\n  optional() { this.#mailer.send?.(); }\r\n  put() { this.#items?.add('x'); }\r\n  publicPut() { this.items.add('x'); }\r\n}\r\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Vault::notify",
                expected: json!([
                    {"name": "Vault::notify", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::optional",
                expected: json!([
                    {"name": "Vault::optional", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::put",
                expected: json!([
                    {"name": "Vault::put", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::publicPut",
                expected: json!([
                    {"name": "Vault::publicPut", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// ts-this-field-call.test.ts:145 — keeps optional receivers distinct from public fields (tsx, LF)
#[tokio::test]
async fn ts_this_field_call_146_tsx_lf() {
    check_case(Case {
        name: "ts_this_field_call_146_tsx_lf",
        files: vec![
            SourceFile {
                path: "vault.tsx",
                source: "\nexport class Mailer { send() {} }\nexport class Cart { add() {} }\nexport class Vault {\n  #mailer = new Mailer();\n  #items = new Set();\n  items = new Cart();\n  notify() { this.#mailer?.send(); }\n  optional() { this.#mailer.send?.(); }\n  put() { this.#items?.add('x'); }\n  publicPut() { this.items.add('x'); }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Vault::notify",
                expected: json!([
                    {"name": "Vault::notify", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::optional",
                expected: json!([
                    {"name": "Vault::optional", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::put",
                expected: json!([
                    {"name": "Vault::put", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::publicPut",
                expected: json!([
                    {"name": "Vault::publicPut", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// ts-this-field-call.test.ts:145 — keeps optional receivers distinct from public fields (tsx, CRLF)
#[tokio::test]
async fn ts_this_field_call_146_tsx_crlf() {
    check_case(Case {
        name: "ts_this_field_call_146_tsx_crlf",
        files: vec![
            SourceFile {
                path: "vault.tsx",
                source: "\r\nexport class Mailer { send() {} }\r\nexport class Cart { add() {} }\r\nexport class Vault {\r\n  #mailer = new Mailer();\r\n  #items = new Set();\r\n  items = new Cart();\r\n  notify() { this.#mailer?.send(); }\r\n  optional() { this.#mailer.send?.(); }\r\n  put() { this.#items?.add('x'); }\r\n  publicPut() { this.items.add('x'); }\r\n}\r\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Vault::notify",
                expected: json!([
                    {"name": "Vault::notify", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::optional",
                expected: json!([
                    {"name": "Vault::optional", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::put",
                expected: json!([
                    {"name": "Vault::put", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::publicPut",
                expected: json!([
                    {"name": "Vault::publicPut", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// ts-this-field-call.test.ts:145 — keeps optional receivers distinct from public fields (js, LF)
#[tokio::test]
async fn ts_this_field_call_146_js_lf() {
    check_case(Case {
        name: "ts_this_field_call_146_js_lf",
        files: vec![
            SourceFile {
                path: "vault.js",
                source: "\nexport class Mailer { send() {} }\nexport class Cart { add() {} }\nexport class Vault {\n  #mailer = new Mailer();\n  #items = new Set();\n  items = new Cart();\n  notify() { this.#mailer?.send(); }\n  optional() { this.#mailer.send?.(); }\n  put() { this.#items?.add('x'); }\n  publicPut() { this.items.add('x'); }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Vault::notify",
                expected: json!([
                    {"name": "Vault::notify", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::optional",
                expected: json!([
                    {"name": "Vault::optional", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::put",
                expected: json!([
                    {"name": "Vault::put", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::publicPut",
                expected: json!([
                    {"name": "Vault::publicPut", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// ts-this-field-call.test.ts:145 — keeps optional receivers distinct from public fields (js, CRLF)
#[tokio::test]
async fn ts_this_field_call_146_js_crlf() {
    check_case(Case {
        name: "ts_this_field_call_146_js_crlf",
        files: vec![
            SourceFile {
                path: "vault.js",
                source: "\r\nexport class Mailer { send() {} }\r\nexport class Cart { add() {} }\r\nexport class Vault {\r\n  #mailer = new Mailer();\r\n  #items = new Set();\r\n  items = new Cart();\r\n  notify() { this.#mailer?.send(); }\r\n  optional() { this.#mailer.send?.(); }\r\n  put() { this.#items?.add('x'); }\r\n  publicPut() { this.items.add('x'); }\r\n}\r\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Vault::notify",
                expected: json!([
                    {"name": "Vault::notify", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::optional",
                expected: json!([
                    {"name": "Vault::optional", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::put",
                expected: json!([
                    {"name": "Vault::put", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::publicPut",
                expected: json!([
                    {"name": "Vault::publicPut", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// ts-this-field-call.test.ts:145 — keeps optional receivers distinct from public fields (jsx, LF)
#[tokio::test]
async fn ts_this_field_call_146_jsx_lf() {
    check_case(Case {
        name: "ts_this_field_call_146_jsx_lf",
        files: vec![
            SourceFile {
                path: "vault.jsx",
                source: "\nexport class Mailer { send() {} }\nexport class Cart { add() {} }\nexport class Vault {\n  #mailer = new Mailer();\n  #items = new Set();\n  items = new Cart();\n  notify() { this.#mailer?.send(); }\n  optional() { this.#mailer.send?.(); }\n  put() { this.#items?.add('x'); }\n  publicPut() { this.items.add('x'); }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Vault::notify",
                expected: json!([
                    {"name": "Vault::notify", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::optional",
                expected: json!([
                    {"name": "Vault::optional", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::put",
                expected: json!([
                    {"name": "Vault::put", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::publicPut",
                expected: json!([
                    {"name": "Vault::publicPut", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// ts-this-field-call.test.ts:145 — keeps optional receivers distinct from public fields (jsx, CRLF)
#[tokio::test]
async fn ts_this_field_call_146_jsx_crlf() {
    check_case(Case {
        name: "ts_this_field_call_146_jsx_crlf",
        files: vec![
            SourceFile {
                path: "vault.jsx",
                source: "\r\nexport class Mailer { send() {} }\r\nexport class Cart { add() {} }\r\nexport class Vault {\r\n  #mailer = new Mailer();\r\n  #items = new Set();\r\n  items = new Cart();\r\n  notify() { this.#mailer?.send(); }\r\n  optional() { this.#mailer.send?.(); }\r\n  put() { this.#items?.add('x'); }\r\n  publicPut() { this.items.add('x'); }\r\n}\r\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Vault::notify",
                expected: json!([
                    {"name": "Vault::notify", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::optional",
                expected: json!([
                    {"name": "Vault::optional", "edges": [
                        {"symbol": {"name": "Mailer::send"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::put",
                expected: json!([
                    {"name": "Vault::put", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Vault::publicPut",
                expected: json!([
                    {"name": "Vault::publicPut", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// python-module-scope-collection-methods.test.ts:86 — does not connect DEFAULTS.get to the unrelated LRUCache.get method
// python-module-scope-collection-methods.test.ts:90 — keeps dict_literal external
// python-module-scope-collection-methods.test.ts:90 — keeps empty_dict external
// python-module-scope-collection-methods.test.ts:90 — keeps dict_constructor external
// python-module-scope-collection-methods.test.ts:90 — keeps list_literal external
// python-module-scope-collection-methods.test.ts:90 — keeps empty_list external
// python-module-scope-collection-methods.test.ts:90 — keeps list_constructor external
// python-module-scope-collection-methods.test.ts:90 — keeps set_literal external
// python-module-scope-collection-methods.test.ts:90 — keeps set_constructor external
// python-module-scope-collection-methods.test.ts:90 — keeps tuple_literal external
// python-module-scope-collection-methods.test.ts:90 — keeps empty_tuple external
// python-module-scope-collection-methods.test.ts:90 — keeps tuple_constructor external
// python-module-scope-collection-methods.test.ts:90 — keeps frozenset_constructor external
// python-module-scope-collection-methods.test.ts:94 — does not treat an unrelated variable as evidence of a project class
// python-module-scope-collection-methods.test.ts:98 — preserves real instance calls despite same-named collections in other files
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn python_module_scope_collection_methods_86() {
    check_case(Case {
        name: "python_module_scope_collection_methods_86",
        files: vec![
            SourceFile {
                path: "settings.py",
                source: "DEFAULTS = {\"answer\": \"42\"}\n\ndef read_setting(name):\n    return DEFAULTS.get(name, None)\n",
            },
            SourceFile {
                path: "cache.py",
                source: "class LRUCache:\n    def __init__(self):\n        self._store = {}\n\n    def get(self, key):\n        return self._store.get(key)\n\nclass ProjectCollection:\n    def append(self, item):\n        pass\n    def add(self, item):\n        pass\n    def index(self, item):\n        return 0\n    def union(self, item):\n        return self\n",
            },
            SourceFile {
                path: "unknown.py",
                source: "UNKNOWN = load_defaults()\n\ndef read_unknown(name):\n    return UNKNOWN.get(name)\n",
            },
            SourceFile {
                path: "client.py",
                source: "from cache import LRUCache\n\ndef read_cache(lRUCache: LRUCache, name):\n    return lRUCache.get(name)\n",
            },
            SourceFile {
                path: "dict_literal.py",
                source: "lRUCache = {\"answer\": \"42\"}\n\ndef use_dict_literal(item):\n    return lRUCache.get(item)\n",
            },
            SourceFile {
                path: "empty_dict.py",
                source: "lRUCache = {}\n\ndef use_empty_dict(item):\n    return lRUCache.get(item)\n",
            },
            SourceFile {
                path: "dict_constructor.py",
                source: "lRUCache = dict()\n\ndef use_dict_constructor(item):\n    return lRUCache.get(item)\n",
            },
            SourceFile {
                path: "list_literal.py",
                source: "lRUCache = [1]\n\ndef use_list_literal(item):\n    return lRUCache.append(item)\n",
            },
            SourceFile {
                path: "empty_list.py",
                source: "lRUCache = []\n\ndef use_empty_list(item):\n    return lRUCache.append(item)\n",
            },
            SourceFile {
                path: "list_constructor.py",
                source: "lRUCache = list()\n\ndef use_list_constructor(item):\n    return lRUCache.append(item)\n",
            },
            SourceFile {
                path: "set_literal.py",
                source: "lRUCache = {1}\n\ndef use_set_literal(item):\n    return lRUCache.add(item)\n",
            },
            SourceFile {
                path: "set_constructor.py",
                source: "lRUCache = set()\n\ndef use_set_constructor(item):\n    return lRUCache.add(item)\n",
            },
            SourceFile {
                path: "tuple_literal.py",
                source: "lRUCache = (1,)\n\ndef use_tuple_literal(item):\n    return lRUCache.index(item)\n",
            },
            SourceFile {
                path: "empty_tuple.py",
                source: "lRUCache = ()\n\ndef use_empty_tuple(item):\n    return lRUCache.index(item)\n",
            },
            SourceFile {
                path: "tuple_constructor.py",
                source: "lRUCache = tuple()\n\ndef use_tuple_constructor(item):\n    return lRUCache.index(item)\n",
            },
            SourceFile {
                path: "frozenset_constructor.py",
                source: "lRUCache = frozenset()\n\ndef use_frozenset_constructor(item):\n    return lRUCache.union(item)\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "LRUCache::get",
                expected: json!([
                    {"name": "LRUCache::get", "edges": [
                        {"symbol": {"name": "read_setting", "filePath": "settings.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "read_setting",
                expected: json!([
                    {"name": "read_setting", "edges": [
                        {"symbol": {"name": "LRUCache::get", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "LRUCache::get",
                expected: json!([
                    {"name": "LRUCache::get", "edges": [
                        {"symbol": {"name": "use_dict_literal", "filePath": "dict_literal.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_dict_literal",
                expected: json!([
                    {"name": "use_dict_literal", "edges": [
                        {"symbol": {"name": "LRUCache::get", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "LRUCache::get",
                expected: json!([
                    {"name": "LRUCache::get", "edges": [
                        {"symbol": {"name": "use_empty_dict", "filePath": "empty_dict.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_empty_dict",
                expected: json!([
                    {"name": "use_empty_dict", "edges": [
                        {"symbol": {"name": "LRUCache::get", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "LRUCache::get",
                expected: json!([
                    {"name": "LRUCache::get", "edges": [
                        {"symbol": {"name": "use_dict_constructor", "filePath": "dict_constructor.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_dict_constructor",
                expected: json!([
                    {"name": "use_dict_constructor", "edges": [
                        {"symbol": {"name": "LRUCache::get", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "ProjectCollection::append",
                expected: json!([
                    {"name": "ProjectCollection::append", "edges": [
                        {"symbol": {"name": "use_list_literal", "filePath": "list_literal.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_list_literal",
                expected: json!([
                    {"name": "use_list_literal", "edges": [
                        {"symbol": {"name": "ProjectCollection::append", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "ProjectCollection::append",
                expected: json!([
                    {"name": "ProjectCollection::append", "edges": [
                        {"symbol": {"name": "use_empty_list", "filePath": "empty_list.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_empty_list",
                expected: json!([
                    {"name": "use_empty_list", "edges": [
                        {"symbol": {"name": "ProjectCollection::append", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "ProjectCollection::append",
                expected: json!([
                    {"name": "ProjectCollection::append", "edges": [
                        {"symbol": {"name": "use_list_constructor", "filePath": "list_constructor.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_list_constructor",
                expected: json!([
                    {"name": "use_list_constructor", "edges": [
                        {"symbol": {"name": "ProjectCollection::append", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "ProjectCollection::add",
                expected: json!([
                    {"name": "ProjectCollection::add", "edges": [
                        {"symbol": {"name": "use_set_literal", "filePath": "set_literal.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_set_literal",
                expected: json!([
                    {"name": "use_set_literal", "edges": [
                        {"symbol": {"name": "ProjectCollection::add", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "ProjectCollection::add",
                expected: json!([
                    {"name": "ProjectCollection::add", "edges": [
                        {"symbol": {"name": "use_set_constructor", "filePath": "set_constructor.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_set_constructor",
                expected: json!([
                    {"name": "use_set_constructor", "edges": [
                        {"symbol": {"name": "ProjectCollection::add", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "ProjectCollection::index",
                expected: json!([
                    {"name": "ProjectCollection::index", "edges": [
                        {"symbol": {"name": "use_tuple_literal", "filePath": "tuple_literal.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_tuple_literal",
                expected: json!([
                    {"name": "use_tuple_literal", "edges": [
                        {"symbol": {"name": "ProjectCollection::index", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "ProjectCollection::index",
                expected: json!([
                    {"name": "ProjectCollection::index", "edges": [
                        {"symbol": {"name": "use_empty_tuple", "filePath": "empty_tuple.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_empty_tuple",
                expected: json!([
                    {"name": "use_empty_tuple", "edges": [
                        {"symbol": {"name": "ProjectCollection::index", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "ProjectCollection::index",
                expected: json!([
                    {"name": "ProjectCollection::index", "edges": [
                        {"symbol": {"name": "use_tuple_constructor", "filePath": "tuple_constructor.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_tuple_constructor",
                expected: json!([
                    {"name": "use_tuple_constructor", "edges": [
                        {"symbol": {"name": "ProjectCollection::index", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "ProjectCollection::union",
                expected: json!([
                    {"name": "ProjectCollection::union", "edges": [
                        {"symbol": {"name": "use_frozenset_constructor", "filePath": "frozenset_constructor.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_frozenset_constructor",
                expected: json!([
                    {"name": "use_frozenset_constructor", "edges": [
                        {"symbol": {"name": "ProjectCollection::union", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "LRUCache::get",
                expected: json!([
                    {"name": "LRUCache::get", "edges": [
                        {"symbol": {"name": "read_unknown", "filePath": "unknown.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "read_unknown",
                expected: json!([
                    {"name": "read_unknown", "edges": [
                        {"symbol": {"name": "LRUCache::get", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "LRUCache::get",
                expected: json!([
                    {"name": "LRUCache::get", "edges": [
                        {"symbol": {"name": "read_cache", "filePath": "client.py"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "read_cache",
                expected: json!([
                    {"name": "read_cache", "edges": [
                        {"symbol": {"name": "LRUCache::get", "filePath": "cache.py"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// expression-receiver-calls.test.ts:82 — TS: a call-result receiver does not bind `.map` to a project method
// expression-receiver-calls.test.ts:87 — Rust: `v.iter().map()` does not bind to a TypeScript class method
// expression-receiver-calls.test.ts:92 — keeps receiver-typed, bare, constructor, this/super and TS→JS calls
#[tokio::test]
async fn expression_receiver_calls_82() {
    check_case(Case {
        name: "expression_receiver_calls_82",
        files: vec![
            SourceFile {
                path: "src/adapter.ts",
                source: "export class GraphAdapter { async map(req?: string) { return req ?? ''; } }\nexport class Runner { run() { return 1; } }\nexport class Base { hello() { return 1; } }\n",
            },
            SourceFile {
                path: "src/use.ts",
                source: "import { GraphAdapter, Runner, Base } from './adapter';\nimport { helper } from './helper.js';\nasync function list(): Promise<string[]> { return []; }\nexport async function names() {\n  const a = (await list()).map((d) => d.length);\n  const b = [1, 2].map((x) => x + 1);\n  const c = (a ?? []).map((x) => x);\n  return [a, b, c];\n}\nexport function typed(g: GraphAdapter | undefined) {\n  return g!.map();\n}\nexport function fresh() { return new Runner().run() + helper(); }\nexport class Child extends Base {\n  greet() { return this.hello() + super.hello(); }\n}\n",
            },
            SourceFile {
                path: "src/helper.js",
                source: "export function helper() { return 2; }\n",
            },
            SourceFile {
                path: "k/src/lib.rs",
                source: "pub fn lens(v: Vec<String>) -> Vec<usize> { v.iter().map(|s| s.len()).collect() }\npub fn opt(o: Option<u8>) -> Option<u16> { o.map(|x| x as u16) }\n",
            },
            SourceFile {
                path: "export_ios.go",
                source: "package main\n\nimport \"C\"\n\n//export OpenFluxStop\nfunc OpenFluxStop() {}\n",
            },
            SourceFile {
                path: "web/min.js",
                source: "function i(a) { return a; }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "names",
                expected: json!([
                    {"name": "names", "edges": [
                        {"symbol": {"name": "list"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "GraphAdapter::map",
                expected: json!([
                    {"name": "GraphAdapter::map", "edges": [
                        {"symbol": {"name": "typed"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "lens",
                expected: json!([
                    {"name": "lens", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "opt",
                expected: json!([
                    {"name": "opt", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "typed",
                expected: json!([
                    {"name": "typed", "edges": [
                        {"symbol": {"name": "GraphAdapter::map"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fresh",
                expected: json!([
                    {"name": "fresh", "edges": [
                        {"symbol": {"name": "Runner::run"}},
                        {"symbol": {"name": "helper"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// awaited-receiver.test.ts:23 — follows an imported factory alias rather than an unrelated namesake (#1840)
#[tokio::test]
async fn awaited_receiver_23() {
    check_case(Case {
        name: "awaited_receiver_23",
        files: vec![
            SourceFile {
                path: "engine.ts",
                source: "export class Engine { run() {} }\nexport async function makeEngine(): Promise<Engine> { return new Engine(); }",
            },
            SourceFile {
                path: "decoy.ts",
                source: "export async function load(): Promise<string> { return \"\"; }",
            },
            SourceFile {
                path: "caller.ts",
                source: "import { makeEngine as load } from \"./engine\";\nexport async function drive() { const handle = await load(); handle.run(); }",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "drive",
                expected: json!([
                    {"filePath": "caller.ts", "edges": [
                        {"symbol": {"name": "Engine::run", "filePath": "engine.ts"}},
                        {"symbol": {"name": "makeEngine", "filePath": "engine.ts"}},
                    ]},
                ]),
                file: Some("caller.ts"),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// awaited-receiver.test.ts:32 — resolves return-type aliases in the factory module, not a caller-local decoy (#1840)
#[tokio::test]
async fn awaited_receiver_32() {
    check_case(Case {
        name: "awaited_receiver_32",
        files: vec![
            SourceFile {
                path: "engine.ts",
                source: "export class Engine { run() {} }\nexport async function makeEngine(): Promise<Engine> { return new Engine(); }",
            },
            SourceFile {
                path: "factory.ts",
                source: "import { Engine as Service } from \"./engine\";\nexport async function load(): Promise<Service> { return new Service(); }",
            },
            SourceFile {
                path: "caller.ts",
                source: "import { load } from \"./factory\";\nclass Service { run() {} }\nexport async function drive() { const handle = await load(); handle.run(); }",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "drive",
                expected: json!([
                    {"filePath": "caller.ts", "edges": [
                        {"symbol": {"name": "Engine::run", "filePath": "engine.ts"}},
                        {"symbol": {"name": "load", "filePath": "factory.ts"}},
                    ]},
                ]),
                file: Some("caller.ts"),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// awaited-receiver.test.ts:41 — does not infer from a factory hidden by a parameter (#1840)
#[tokio::test]
async fn awaited_receiver_41() {
    check_case(Case {
        name: "awaited_receiver_41",
        files: vec![
            SourceFile {
                path: "engine.ts",
                source: "export class Engine { run() {} }\nexport async function makeEngine(): Promise<Engine> { return new Engine(); }",
            },
            SourceFile {
                path: "caller.ts",
                source: "import { makeEngine } from \"./engine\";\nexport async function drive(makeEngine: () => Promise<string>) { const handle = await makeEngine(); handle.run(); }",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "drive",
                expected: json!([
                    {"filePath": "caller.ts", "edges": [
                        {"symbol": {"name": "Engine::run", "filePath": "engine.ts"}},
                    ]},
                ]),
                file: Some("caller.ts"),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// awaited-receiver.test.ts:49 — distinguishes same-named awaited receivers in sibling blocks (#1840)
#[tokio::test]
async fn awaited_receiver_49() {
    check_case(Case {
        name: "awaited_receiver_49",
        files: vec![
            SourceFile {
                path: "caller.ts",
                source: "class PaneManager { split() {} }\nasync function text(): Promise<string> { return \"\"; }\nasync function pane(): Promise<PaneManager> { return new PaneManager(); }\nexport async function drive() {\n  { const value = await text(); value.split(); }\n  { const value = await pane(); value.split(); }\n}",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "drive",
                expected: json!([
                    {"name": "drive", "edges": [
                        {"symbol": {"name": "PaneManager::split", "filePath": "caller.ts"}, "line": 6},
                    ]},
                ]),
                endpoint_suffix: Some("PaneManager::split"),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// awaited-receiver.test.ts:62 — keeps captured awaited bindings but rejects shadowing parameters (#1840)
#[tokio::test]
async fn awaited_receiver_62() {
    check_case(Case {
        name: "awaited_receiver_62",
        files: vec![
            SourceFile {
                path: "caller.ts",
                source: "export class Engine { run() {} }\nexport async function makeEngine(): Promise<Engine> { return new Engine(); }\nexport async function outer() {\n  const handle = await makeEngine();\n  function captured() { handle.run(); }\n  function shadow(handle: any) { handle.run(); }\n  return { captured, shadow };\n}",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "captured",
                expected: json!([
                    {"filePath": "caller.ts", "edges": [
                        {"symbol": {"name": "Engine::run", "filePath": "caller.ts"}},
                    ]},
                ]),
                file: Some("caller.ts"),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "shadow",
                expected: json!([
                    {"filePath": "caller.ts", "edges": [
                        {"symbol": {"name": "Engine::run", "filePath": "caller.ts"}},
                    ]},
                ]),
                file: Some("caller.ts"),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// awaited-receiver.test.ts:74 — invalidates an awaited return type after edits in the callee file (#1840)
#[tokio::test]
async fn awaited_receiver_74() {
    check_stages(
        &[
            Case {
                name: "invalidates an awaited return type after edits in the callee file (#1840)",
                files: vec![
                    SourceFile {
                        path: "caller.ts",
                        source: "import { load } from \"./factory\";\nexport async function drive() { const value = await load(); value.split(); }",
                    },
                    SourceFile {
                        path: "factory.ts",
                        source: "export async function load(): Promise<string> { return \"\"; }",
                    },
                    SourceFile {
                        path: "decoy.ts",
                        source: "export class Other { split() {} }",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callees",
                        symbol: "drive",
                        expected: json!([
                            {"filePath": "caller.ts", "edges": [
                                {"symbol": {"name": "load", "filePath": "factory.ts"}},
                            ]},
                        ]),
                        file: Some("caller.ts"),
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "invalidates an awaited return type after edits in the callee file (#1840)",
                files: vec![
                    SourceFile {
                        path: "caller.ts",
                        source: "import { load } from \"./factory\";\nexport async function drive() { const value = await load(); value.split(); }",
                    },
                    SourceFile {
                        path: "factory.ts",
                        source: "export class Pane { split() {} }\nexport async function load(): Promise<Pane> { return new Pane(); }",
                    },
                    SourceFile {
                        path: "decoy.ts",
                        source: "export class Other { split() {} }",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callees",
                        symbol: "drive",
                        expected: json!([
                            {"filePath": "caller.ts", "edges": [
                                {"symbol": {"name": "Pane::split", "filePath": "factory.ts"}},
                                {"symbol": {"name": "load", "filePath": "factory.ts"}},
                            ]},
                        ]),
                        file: Some("caller.ts"),
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "invalidates an awaited return type after edits in the callee file (#1840)",
                files: vec![
                    SourceFile {
                        path: "caller.ts",
                        source: "import { load } from \"./factory\";\nexport async function drive() { const value = await load(); value.split(); }",
                    },
                    SourceFile {
                        path: "factory.ts",
                        source: "export async function load(): Promise<string> { return \"\"; }",
                    },
                    SourceFile {
                        path: "decoy.ts",
                        source: "export class Other { split() {} }",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callees",
                        symbol: "drive",
                        expected: json!([
                            {"filePath": "caller.ts", "edges": [
                                {"symbol": {"name": "load", "filePath": "factory.ts"}},
                            ]},
                        ]),
                        file: Some("caller.ts"),
                        ..Query::default()
                    },
                ],
            },
        ],
        false,
    ).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// awaited-receiver.test.ts:88 — reads a multiline factory annotation and preserves ordinary typed receivers (#1840)
#[tokio::test]
async fn awaited_receiver_88() {
    check_case(Case {
        name: "awaited_receiver_88",
        files: vec![
            SourceFile {
                path: "caller.ts",
                source: "class Engine { run() {} }\nclass Decoy { run() {} }\nasync function load(\n  input: string\n): Promise<Engine> { return new Engine(); }\nexport async function drive() { const value = await load(''); value.run(); }\nexport function ordinary() { const engine = new Engine(); engine.run(); }",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "drive",
                expected: json!([
                    {"filePath": "caller.ts", "edges": [
                        {"symbol": {"name": "Engine::run", "filePath": "caller.ts"}},
                        {"symbol": {"name": "load", "filePath": "caller.ts"}},
                    ]},
                ]),
                file: Some("caller.ts"),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ordinary",
                expected: json!([
                    {"filePath": "caller.ts", "edges": [
                        {"symbol": {"name": "Engine::run", "filePath": "caller.ts"}},
                    ]},
                ]),
                file: Some("caller.ts"),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// awaited-receiver.test.ts:100 — supports newline-terminated awaited declarations without treating a chained result as the factory type (#1840)
#[tokio::test]
async fn awaited_receiver_100() {
    check_case(Case {
        name: "awaited_receiver_100",
        files: vec![
            SourceFile {
                path: "caller.ts",
                source: "export class Engine { run() {} }\nexport async function makeEngine(): Promise<Engine> { return new Engine(); }\nexport async function drive() {\n  const value = await makeEngine()\n  value.run()\n}\nexport async function chained() {\n  const value = await makeEngine().toString();\n  value.run();\n}",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "drive",
                expected: json!([
                    {"filePath": "caller.ts", "edges": [
                        {"symbol": {"name": "Engine::run", "filePath": "caller.ts"}},
                    ]},
                ]),
                file: Some("caller.ts"),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "chained",
                expected: json!([
                    {"filePath": "caller.ts", "edges": [
                        {"symbol": {"name": "Engine::run", "filePath": "caller.ts"}},
                    ]},
                ]),
                file: Some("caller.ts"),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// awaited-receiver.test.ts:114 — does not borrow a local factory annotation through a nearer variable binding (#1840)
#[tokio::test]
async fn awaited_receiver_114() {
    check_case(Case {
        name: "awaited_receiver_114",
        files: vec![
            SourceFile {
                path: "caller.ts",
                source: "export class Engine { run() {} }\nexport async function makeEngine(): Promise<Engine> { return new Engine(); }\nexport async function drive(other: () => Promise<string>) {\n  const makeEngine = other;\n  const value = await makeEngine();\n  value.run();\n}",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "drive",
                expected: json!([
                    {"filePath": "caller.ts", "edges": [
                        {"symbol": {"name": "Engine::run", "filePath": "caller.ts"}},
                    ]},
                ]),
                file: Some("caller.ts"),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// awaited-receiver.test.ts:124 — preserves a real awaited member-factory call outside the bare-callee inference path (#1840)
#[tokio::test]
async fn awaited_receiver_124() {
    check_case(Case {
        name: "awaited_receiver_124",
        files: vec![
            SourceFile {
                path: "caller.ts",
                source: "class Engine { run() {} }\nclass Factory { async create(): Promise<Engine> { return new Engine(); } }\nexport async function drive() {\n  const handle = await new Factory().create();\n  handle.run();\n}",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "drive",
                expected: json!([
                    {"filePath": "caller.ts", "edges": [
                        {"symbol": {"name": "Engine::run", "filePath": "caller.ts"}},
                    ]},
                ]),
                file: Some("caller.ts"),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// awaited-receiver.test.ts:134 — invalidates negative and positive file eligibility when caller edits add and remove await (#1840)
#[tokio::test]
async fn awaited_receiver_134() {
    check_stages(
        &[
            Case {
                name: "invalidates negative and positive file eligibility when caller edits add and remove await (#1840)",
                files: vec![
                    SourceFile {
                        path: "caller.ts",
                        source: "import { load } from \"./factory\";\nfunction opaque() { return null; }\nexport async function drive() { const value = opaque(); value.split(); }",
                    },
                    SourceFile {
                        path: "factory.ts",
                        source: "export class Pane { split() {} }\nexport class Other { split() {} }\nexport async function load(): Promise<Pane> { return new Pane(); }",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callees",
                        symbol: "drive",
                        expected: json!([
                            {"filePath": "caller.ts", "edges": [
                            ]},
                        ]),
                        file: Some("caller.ts"),
                        endpoint_suffix: Some("Pane::split"),
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "invalidates negative and positive file eligibility when caller edits add and remove await (#1840)",
                files: vec![
                    SourceFile {
                        path: "caller.ts",
                        source: "import { load } from \"./factory\";\nexport async function drive() { const value = await load(); value.split(); }",
                    },
                    SourceFile {
                        path: "factory.ts",
                        source: "export class Pane { split() {} }\nexport class Other { split() {} }\nexport async function load(): Promise<Pane> { return new Pane(); }",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callees",
                        symbol: "drive",
                        expected: json!([
                            {"filePath": "caller.ts", "edges": [
                                {"symbol": {"name": "Pane::split", "filePath": "factory.ts"}},
                            ]},
                        ]),
                        file: Some("caller.ts"),
                        check: Check::Contains,
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "invalidates negative and positive file eligibility when caller edits add and remove await (#1840)",
                files: vec![
                    SourceFile {
                        path: "caller.ts",
                        source: "import { load } from \"./factory\";\nfunction opaque() { return null; }\nexport async function drive() { const value = opaque(); value.split(); }",
                    },
                    SourceFile {
                        path: "factory.ts",
                        source: "export class Pane { split() {} }\nexport class Other { split() {} }\nexport async function load(): Promise<Pane> { return new Pane(); }",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callees",
                        symbol: "drive",
                        expected: json!([
                            {"filePath": "caller.ts", "edges": [
                            ]},
                        ]),
                        file: Some("caller.ts"),
                        endpoint_suffix: Some("Pane::split"),
                        ..Query::default()
                    },
                ],
            },
        ],
        false,
    ).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// python-quoted-annotation.test.ts:50 — resolves the method on the quoted type, the same as the unquoted annotation
#[tokio::test]
async fn python_quoted_annotation_50() {
    check_case(Case {
        name: "python_quoted_annotation_50",
        files: vec![
            SourceFile {
                path: "pkg/__init__.py",
                source: "",
            },
            SourceFile {
                path: "pkg/a.py",
                source: "def render(x):\n    return x\n\nclass Alpha:\n    def render(self):\n        return \"a\"\n\nclass Beta:\n    def render(self):\n        return \"b\"\n",
            },
            SourceFile {
                path: "pkg/b.py",
                source: "from __future__ import annotations\nfrom pkg.a import Alpha, Beta\n\ndef quoted(o: \"Alpha\"):\n    return o.render()\n\ndef single_quoted(o: 'Beta'):\n    return o.render()\n\ndef unquoted(o: Alpha):\n    return o.render()\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "unquoted",
                expected: json!([
                    {"name": "unquoted", "edges": [
                        {"symbol": {"name": "Alpha::render"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "quoted",
                expected: json!([
                    {"name": "quoted", "edges": [
                        {"symbol": {"name": "Alpha::render"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "single_quoted",
                expected: json!([
                    {"name": "single_quoted", "edges": [
                        {"symbol": {"name": "Beta::render"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// call-receiver-no-fabrication.test.ts:57 — Python: no edge from a call-result receiver to a same-named top-level function
// call-receiver-no-fabrication.test.ts:65 — JavaScript: the same shape, and the inner call keeps its edge
#[tokio::test]
async fn call_receiver_no_fabrication_57() {
    check_case(Case {
        name: "call_receiver_no_fabrication_57",
        files: vec![
            SourceFile {
                path: "py/__init__.py",
                source: "",
            },
            SourceFile {
                path: "py/collect.py",
                source: "def append(item):\n    return item\n\ndef get(key):\n    return key\n\ndef make():\n    return {}\n\ndef bucket(d, k, v):\n    d.setdefault(k, []).append(v)\n    return d.items().get(k)\n\ndef fresh():\n    return make().get(\"x\")\n",
            },
            SourceFile {
                path: "js/collect.js",
                source: "function append(item) { return item; }\nfunction run() { return 1; }\nfunction make() { return {}; }\nfunction bucket(d, k, v) { d.setdefault(k, []).append(v); make().run(); (0, make)().run(); }\nmodule.exports = { append, run, make, bucket };\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "bucket",
                expected: json!([
                    {"filePath": "py/collect.py", "edges": [
                    ]},
                ]),
                file: Some("py/collect.py"),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "append",
                expected: json!([
                    {"filePath": "py/collect.py", "edges": [
                    ]},
                ]),
                file: Some("py/collect.py"),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "get",
                expected: json!([
                    {"filePath": "py/collect.py", "edges": [
                    ]},
                ]),
                file: Some("py/collect.py"),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fresh",
                expected: json!([
                    {"filePath": "py/collect.py", "edges": [
                        {"symbol": {"name": "make"}},
                    ]},
                ]),
                file: Some("py/collect.py"),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "append",
                expected: json!([
                    {"filePath": "js/collect.js", "edges": [
                    ]},
                ]),
                file: Some("js/collect.js"),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "run",
                expected: json!([
                    {"filePath": "js/collect.js", "edges": [
                    ]},
                ]),
                file: Some("js/collect.js"),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "bucket",
                expected: json!([
                    {"filePath": "js/collect.js", "edges": [
                        {"symbol": {"name": "make"}},
                    ]},
                ]),
                file: Some("js/collect.js"),
                short_names: true,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// js-builtin-method-calls.test.ts:26 — does not guess project methods for unknown receivers, including class-name overlap
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn js_builtin_method_calls_26_ts() {
    check_case(Case {
        name: "js_builtin_method_calls_26_ts",
        files: vec![
            SourceFile {
                path: "decoys.ts",
                source: "export class arrayWrapper {\n  map() {}\n  filter() {}\n  reduce() {}\n  forEach() {}\n  find() {}\n  findIndex() {}\n  some() {}\n  every() {}\n  push() {}\n  pop() {}\n  slice() {}\n  splice() {}\n  sort() {}\n  flatMap() {}\n  includes() {}\n  join() {}\n}\nexport class collectionWrapper {\n  get() {}\n  set() {}\n  has() {}\n  add() {}\n  delete() {}\n  clear() {}\n  keys() {}\n  values() {}\n  entries() {}\n}\nexport class stringWrapper {\n  trim() {}\n  split() {}\n  replace() {}\n  replaceAll() {}\n  match() {}\n  matchAll() {}\n  search() {}\n  startsWith() {}\n  endsWith() {}\n  substring() {}\n  toLowerCase() {}\n  toUpperCase() {}\n}\nexport class promiseWrapper {\n  then() {}\n  catch() {}\n  finally() {}\n}\nexport class functionWrapper {\n  call() {}\n  apply() {}\n  bind() {}\n}\nexport class eventWrapper {\n  on() {}\n  once() {}\n  off() {}\n  emit() {}\n  addListener() {}\n  removeListener() {}\n  removeAllListeners() {}\n  addEventListener() {}\n  removeEventListener() {}\n  dispatchEvent() {}\n}\nexport class iteratorWrapper {\n  next() {}\n  return() {}\n  throw() {}\n}\n",
            },
            SourceFile {
                path: "calls.ts",
                source: "export function array_map(array) { array.map(); }\nexport function array_filter(array) { array.filter(); }\nexport function array_reduce(array) { array.reduce(); }\nexport function array_forEach(array) { array.forEach(); }\nexport function array_find(array) { array.find(); }\nexport function array_findIndex(array) { array.findIndex(); }\nexport function array_some(array) { array.some(); }\nexport function array_every(array) { array.every(); }\nexport function array_push(array) { array.push(); }\nexport function array_pop(array) { array.pop(); }\nexport function array_slice(array) { array.slice(); }\nexport function array_splice(array) { array.splice(); }\nexport function array_sort(array) { array.sort(); }\nexport function array_flatMap(array) { array.flatMap(); }\nexport function array_includes(array) { array.includes(); }\nexport function array_join(array) { array.join(); }\nexport function collection_get(collection) { collection.get(); }\nexport function collection_set(collection) { collection.set(); }\nexport function collection_has(collection) { collection.has(); }\nexport function collection_add(collection) { collection.add(); }\nexport function collection_delete(collection) { collection.delete(); }\nexport function collection_clear(collection) { collection.clear(); }\nexport function collection_keys(collection) { collection.keys(); }\nexport function collection_values(collection) { collection.values(); }\nexport function collection_entries(collection) { collection.entries(); }\nexport function string_trim(string) { string.trim(); }\nexport function string_split(string) { string.split(); }\nexport function string_replace(string) { string.replace(); }\nexport function string_replaceAll(string) { string.replaceAll(); }\nexport function string_match(string) { string.match(); }\nexport function string_matchAll(string) { string.matchAll(); }\nexport function string_search(string) { string.search(); }\nexport function string_startsWith(string) { string.startsWith(); }\nexport function string_endsWith(string) { string.endsWith(); }\nexport function string_substring(string) { string.substring(); }\nexport function string_toLowerCase(string) { string.toLowerCase(); }\nexport function string_toUpperCase(string) { string.toUpperCase(); }\nexport function promise_then(promise) { promise.then(); }\nexport function promise_catch(promise) { promise.catch(); }\nexport function promise_finally(promise) { promise.finally(); }\nexport function function_call(function) { function.call(); }\nexport function function_apply(function) { function.apply(); }\nexport function function_bind(function) { function.bind(); }\nexport function event_on(event) { event.on(); }\nexport function event_once(event) { event.once(); }\nexport function event_off(event) { event.off(); }\nexport function event_emit(event) { event.emit(); }\nexport function event_addListener(event) { event.addListener(); }\nexport function event_removeListener(event) { event.removeListener(); }\nexport function event_removeAllListeners(event) { event.removeAllListeners(); }\nexport function event_addEventListener(event) { event.addEventListener(); }\nexport function event_removeEventListener(event) { event.removeEventListener(); }\nexport function event_dispatchEvent(event) { event.dispatchEvent(); }\nexport function iterator_next(iterator) { iterator.next(); }\nexport function iterator_return(iterator) { iterator.return(); }\nexport function iterator_throw(iterator) { iterator.throw(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "array_map",
                expected: json!([
                    {"name": "array_map", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_filter",
                expected: json!([
                    {"name": "array_filter", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_reduce",
                expected: json!([
                    {"name": "array_reduce", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_forEach",
                expected: json!([
                    {"name": "array_forEach", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_find",
                expected: json!([
                    {"name": "array_find", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_findIndex",
                expected: json!([
                    {"name": "array_findIndex", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_some",
                expected: json!([
                    {"name": "array_some", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_every",
                expected: json!([
                    {"name": "array_every", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_push",
                expected: json!([
                    {"name": "array_push", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_pop",
                expected: json!([
                    {"name": "array_pop", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_slice",
                expected: json!([
                    {"name": "array_slice", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_splice",
                expected: json!([
                    {"name": "array_splice", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_sort",
                expected: json!([
                    {"name": "array_sort", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_flatMap",
                expected: json!([
                    {"name": "array_flatMap", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_includes",
                expected: json!([
                    {"name": "array_includes", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_join",
                expected: json!([
                    {"name": "array_join", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_get",
                expected: json!([
                    {"name": "collection_get", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_set",
                expected: json!([
                    {"name": "collection_set", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_has",
                expected: json!([
                    {"name": "collection_has", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_add",
                expected: json!([
                    {"name": "collection_add", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_delete",
                expected: json!([
                    {"name": "collection_delete", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_clear",
                expected: json!([
                    {"name": "collection_clear", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_keys",
                expected: json!([
                    {"name": "collection_keys", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_values",
                expected: json!([
                    {"name": "collection_values", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_entries",
                expected: json!([
                    {"name": "collection_entries", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_trim",
                expected: json!([
                    {"name": "string_trim", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_split",
                expected: json!([
                    {"name": "string_split", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_replace",
                expected: json!([
                    {"name": "string_replace", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_replaceAll",
                expected: json!([
                    {"name": "string_replaceAll", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_match",
                expected: json!([
                    {"name": "string_match", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_matchAll",
                expected: json!([
                    {"name": "string_matchAll", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_search",
                expected: json!([
                    {"name": "string_search", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_startsWith",
                expected: json!([
                    {"name": "string_startsWith", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_endsWith",
                expected: json!([
                    {"name": "string_endsWith", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_substring",
                expected: json!([
                    {"name": "string_substring", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_toLowerCase",
                expected: json!([
                    {"name": "string_toLowerCase", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_toUpperCase",
                expected: json!([
                    {"name": "string_toUpperCase", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "promise_then",
                expected: json!([
                    {"name": "promise_then", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "promise_catch",
                expected: json!([
                    {"name": "promise_catch", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "promise_finally",
                expected: json!([
                    {"name": "promise_finally", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "function_call",
                expected: json!([
                    {"name": "function_call", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "function_apply",
                expected: json!([
                    {"name": "function_apply", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "function_bind",
                expected: json!([
                    {"name": "function_bind", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_on",
                expected: json!([
                    {"name": "event_on", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_once",
                expected: json!([
                    {"name": "event_once", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_off",
                expected: json!([
                    {"name": "event_off", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_emit",
                expected: json!([
                    {"name": "event_emit", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_addListener",
                expected: json!([
                    {"name": "event_addListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_removeListener",
                expected: json!([
                    {"name": "event_removeListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_removeAllListeners",
                expected: json!([
                    {"name": "event_removeAllListeners", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_addEventListener",
                expected: json!([
                    {"name": "event_addEventListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_removeEventListener",
                expected: json!([
                    {"name": "event_removeEventListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_dispatchEvent",
                expected: json!([
                    {"name": "event_dispatchEvent", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "iterator_next",
                expected: json!([
                    {"name": "iterator_next", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "iterator_return",
                expected: json!([
                    {"name": "iterator_return", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "iterator_throw",
                expected: json!([
                    {"name": "iterator_throw", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// js-builtin-method-calls.test.ts:68 — rejects built-in values but retains constructed, imported, literal and class receivers
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn js_builtin_method_calls_68_ts() {
    check_case(Case {
        name: "js_builtin_method_calls_68_ts",
        files: vec![
            SourceFile {
                path: "cart.ts",
                source: "\nexport class Cart {\n  map() {}\n  add() {}\n  get() {}\n  static bind() {}\n  self() { this.add(); }\n}\nexport const api = { map() {} };\nexport class LRUCache { get() {} }\n",
            },
            SourceFile {
                path: "calls.ts",
                source: "\nimport { Cart, api } from './cart';\nexport function tidy(list: string[]) { list.map(); }\nexport function cached() {\n  const cache = new Map();\n  cache.get('x');\n}\nexport function unknownCache(cache) { cache.get('x'); }\nexport function literalArray() { const cart = []; cart.map(); }\nexport function literalString() { const cart = 'x'; cart.trim(); }\nexport function constructed() {\n  const cart = new Cart();\n  cart.add();\n}\nexport function imported() { Cart.bind(); }\nexport function objectLiteral() { api.map(); }\nexport const local = { map() {} };\nexport function localLiteral() { local.map(); }\nexport function typed(cart: Cart) { cart.map(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "tidy",
                expected: json!([
                    {"name": "tidy", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "cached",
                expected: json!([
                    {"name": "cached", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "unknownCache",
                expected: json!([
                    {"name": "unknownCache", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "literalArray",
                expected: json!([
                    {"name": "literalArray", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "literalString",
                expected: json!([
                    {"name": "literalString", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "constructed",
                expected: json!([
                    {"name": "constructed", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "imported",
                expected: json!([
                    {"name": "imported", "edges": [
                        {"symbol": {"name": "Cart::bind"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "objectLiteral",
                expected: json!([
                    {"name": "objectLiteral", "edges": [
                        {"symbol": {"name": "api::map"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "localLiteral",
                expected: json!([
                    {"name": "localLiteral", "edges": [
                        {"symbol": {"name": "local::map"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Cart::self",
                expected: json!([
                    {"name": "Cart::self", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "typed",
                expected: json!([
                    {"name": "typed", "edges": [
                        {"symbol": {"name": "Cart::map"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// js-builtin-method-calls.test.ts:26 — does not guess project methods for unknown receivers, including class-name overlap
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn js_builtin_method_calls_26_tsx() {
    check_case(Case {
        name: "js_builtin_method_calls_26_tsx",
        files: vec![
            SourceFile {
                path: "decoys.tsx",
                source: "export class arrayWrapper {\n  map() {}\n  filter() {}\n  reduce() {}\n  forEach() {}\n  find() {}\n  findIndex() {}\n  some() {}\n  every() {}\n  push() {}\n  pop() {}\n  slice() {}\n  splice() {}\n  sort() {}\n  flatMap() {}\n  includes() {}\n  join() {}\n}\nexport class collectionWrapper {\n  get() {}\n  set() {}\n  has() {}\n  add() {}\n  delete() {}\n  clear() {}\n  keys() {}\n  values() {}\n  entries() {}\n}\nexport class stringWrapper {\n  trim() {}\n  split() {}\n  replace() {}\n  replaceAll() {}\n  match() {}\n  matchAll() {}\n  search() {}\n  startsWith() {}\n  endsWith() {}\n  substring() {}\n  toLowerCase() {}\n  toUpperCase() {}\n}\nexport class promiseWrapper {\n  then() {}\n  catch() {}\n  finally() {}\n}\nexport class functionWrapper {\n  call() {}\n  apply() {}\n  bind() {}\n}\nexport class eventWrapper {\n  on() {}\n  once() {}\n  off() {}\n  emit() {}\n  addListener() {}\n  removeListener() {}\n  removeAllListeners() {}\n  addEventListener() {}\n  removeEventListener() {}\n  dispatchEvent() {}\n}\nexport class iteratorWrapper {\n  next() {}\n  return() {}\n  throw() {}\n}\n",
            },
            SourceFile {
                path: "calls.tsx",
                source: "export function array_map(array) { array.map(); }\nexport function array_filter(array) { array.filter(); }\nexport function array_reduce(array) { array.reduce(); }\nexport function array_forEach(array) { array.forEach(); }\nexport function array_find(array) { array.find(); }\nexport function array_findIndex(array) { array.findIndex(); }\nexport function array_some(array) { array.some(); }\nexport function array_every(array) { array.every(); }\nexport function array_push(array) { array.push(); }\nexport function array_pop(array) { array.pop(); }\nexport function array_slice(array) { array.slice(); }\nexport function array_splice(array) { array.splice(); }\nexport function array_sort(array) { array.sort(); }\nexport function array_flatMap(array) { array.flatMap(); }\nexport function array_includes(array) { array.includes(); }\nexport function array_join(array) { array.join(); }\nexport function collection_get(collection) { collection.get(); }\nexport function collection_set(collection) { collection.set(); }\nexport function collection_has(collection) { collection.has(); }\nexport function collection_add(collection) { collection.add(); }\nexport function collection_delete(collection) { collection.delete(); }\nexport function collection_clear(collection) { collection.clear(); }\nexport function collection_keys(collection) { collection.keys(); }\nexport function collection_values(collection) { collection.values(); }\nexport function collection_entries(collection) { collection.entries(); }\nexport function string_trim(string) { string.trim(); }\nexport function string_split(string) { string.split(); }\nexport function string_replace(string) { string.replace(); }\nexport function string_replaceAll(string) { string.replaceAll(); }\nexport function string_match(string) { string.match(); }\nexport function string_matchAll(string) { string.matchAll(); }\nexport function string_search(string) { string.search(); }\nexport function string_startsWith(string) { string.startsWith(); }\nexport function string_endsWith(string) { string.endsWith(); }\nexport function string_substring(string) { string.substring(); }\nexport function string_toLowerCase(string) { string.toLowerCase(); }\nexport function string_toUpperCase(string) { string.toUpperCase(); }\nexport function promise_then(promise) { promise.then(); }\nexport function promise_catch(promise) { promise.catch(); }\nexport function promise_finally(promise) { promise.finally(); }\nexport function function_call(function) { function.call(); }\nexport function function_apply(function) { function.apply(); }\nexport function function_bind(function) { function.bind(); }\nexport function event_on(event) { event.on(); }\nexport function event_once(event) { event.once(); }\nexport function event_off(event) { event.off(); }\nexport function event_emit(event) { event.emit(); }\nexport function event_addListener(event) { event.addListener(); }\nexport function event_removeListener(event) { event.removeListener(); }\nexport function event_removeAllListeners(event) { event.removeAllListeners(); }\nexport function event_addEventListener(event) { event.addEventListener(); }\nexport function event_removeEventListener(event) { event.removeEventListener(); }\nexport function event_dispatchEvent(event) { event.dispatchEvent(); }\nexport function iterator_next(iterator) { iterator.next(); }\nexport function iterator_return(iterator) { iterator.return(); }\nexport function iterator_throw(iterator) { iterator.throw(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "array_map",
                expected: json!([
                    {"name": "array_map", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_filter",
                expected: json!([
                    {"name": "array_filter", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_reduce",
                expected: json!([
                    {"name": "array_reduce", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_forEach",
                expected: json!([
                    {"name": "array_forEach", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_find",
                expected: json!([
                    {"name": "array_find", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_findIndex",
                expected: json!([
                    {"name": "array_findIndex", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_some",
                expected: json!([
                    {"name": "array_some", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_every",
                expected: json!([
                    {"name": "array_every", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_push",
                expected: json!([
                    {"name": "array_push", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_pop",
                expected: json!([
                    {"name": "array_pop", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_slice",
                expected: json!([
                    {"name": "array_slice", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_splice",
                expected: json!([
                    {"name": "array_splice", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_sort",
                expected: json!([
                    {"name": "array_sort", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_flatMap",
                expected: json!([
                    {"name": "array_flatMap", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_includes",
                expected: json!([
                    {"name": "array_includes", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_join",
                expected: json!([
                    {"name": "array_join", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_get",
                expected: json!([
                    {"name": "collection_get", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_set",
                expected: json!([
                    {"name": "collection_set", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_has",
                expected: json!([
                    {"name": "collection_has", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_add",
                expected: json!([
                    {"name": "collection_add", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_delete",
                expected: json!([
                    {"name": "collection_delete", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_clear",
                expected: json!([
                    {"name": "collection_clear", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_keys",
                expected: json!([
                    {"name": "collection_keys", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_values",
                expected: json!([
                    {"name": "collection_values", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_entries",
                expected: json!([
                    {"name": "collection_entries", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_trim",
                expected: json!([
                    {"name": "string_trim", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_split",
                expected: json!([
                    {"name": "string_split", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_replace",
                expected: json!([
                    {"name": "string_replace", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_replaceAll",
                expected: json!([
                    {"name": "string_replaceAll", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_match",
                expected: json!([
                    {"name": "string_match", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_matchAll",
                expected: json!([
                    {"name": "string_matchAll", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_search",
                expected: json!([
                    {"name": "string_search", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_startsWith",
                expected: json!([
                    {"name": "string_startsWith", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_endsWith",
                expected: json!([
                    {"name": "string_endsWith", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_substring",
                expected: json!([
                    {"name": "string_substring", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_toLowerCase",
                expected: json!([
                    {"name": "string_toLowerCase", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_toUpperCase",
                expected: json!([
                    {"name": "string_toUpperCase", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "promise_then",
                expected: json!([
                    {"name": "promise_then", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "promise_catch",
                expected: json!([
                    {"name": "promise_catch", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "promise_finally",
                expected: json!([
                    {"name": "promise_finally", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "function_call",
                expected: json!([
                    {"name": "function_call", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "function_apply",
                expected: json!([
                    {"name": "function_apply", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "function_bind",
                expected: json!([
                    {"name": "function_bind", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_on",
                expected: json!([
                    {"name": "event_on", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_once",
                expected: json!([
                    {"name": "event_once", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_off",
                expected: json!([
                    {"name": "event_off", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_emit",
                expected: json!([
                    {"name": "event_emit", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_addListener",
                expected: json!([
                    {"name": "event_addListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_removeListener",
                expected: json!([
                    {"name": "event_removeListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_removeAllListeners",
                expected: json!([
                    {"name": "event_removeAllListeners", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_addEventListener",
                expected: json!([
                    {"name": "event_addEventListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_removeEventListener",
                expected: json!([
                    {"name": "event_removeEventListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_dispatchEvent",
                expected: json!([
                    {"name": "event_dispatchEvent", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "iterator_next",
                expected: json!([
                    {"name": "iterator_next", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "iterator_return",
                expected: json!([
                    {"name": "iterator_return", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "iterator_throw",
                expected: json!([
                    {"name": "iterator_throw", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// js-builtin-method-calls.test.ts:68 — rejects built-in values but retains constructed, imported, literal and class receivers
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn js_builtin_method_calls_68_tsx() {
    check_case(Case {
        name: "js_builtin_method_calls_68_tsx",
        files: vec![
            SourceFile {
                path: "cart.tsx",
                source: "\nexport class Cart {\n  map() {}\n  add() {}\n  get() {}\n  static bind() {}\n  self() { this.add(); }\n}\nexport const api = { map() {} };\nexport class LRUCache { get() {} }\n",
            },
            SourceFile {
                path: "calls.tsx",
                source: "\nimport { Cart, api } from './cart';\nexport function tidy(list: string[]) { list.map(); }\nexport function cached() {\n  const cache = new Map();\n  cache.get('x');\n}\nexport function unknownCache(cache) { cache.get('x'); }\nexport function literalArray() { const cart = []; cart.map(); }\nexport function literalString() { const cart = 'x'; cart.trim(); }\nexport function constructed() {\n  const cart = new Cart();\n  cart.add();\n}\nexport function imported() { Cart.bind(); }\nexport function objectLiteral() { api.map(); }\nexport const local = { map() {} };\nexport function localLiteral() { local.map(); }\nexport function typed(cart: Cart) { cart.map(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "tidy",
                expected: json!([
                    {"name": "tidy", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "cached",
                expected: json!([
                    {"name": "cached", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "unknownCache",
                expected: json!([
                    {"name": "unknownCache", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "literalArray",
                expected: json!([
                    {"name": "literalArray", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "literalString",
                expected: json!([
                    {"name": "literalString", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "constructed",
                expected: json!([
                    {"name": "constructed", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "imported",
                expected: json!([
                    {"name": "imported", "edges": [
                        {"symbol": {"name": "Cart::bind"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "objectLiteral",
                expected: json!([
                    {"name": "objectLiteral", "edges": [
                        {"symbol": {"name": "api::map"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "localLiteral",
                expected: json!([
                    {"name": "localLiteral", "edges": [
                        {"symbol": {"name": "local::map"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Cart::self",
                expected: json!([
                    {"name": "Cart::self", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "typed",
                expected: json!([
                    {"name": "typed", "edges": [
                        {"symbol": {"name": "Cart::map"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// js-builtin-method-calls.test.ts:26 — does not guess project methods for unknown receivers, including class-name overlap
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn js_builtin_method_calls_26_js() {
    check_case(Case {
        name: "js_builtin_method_calls_26_js",
        files: vec![
            SourceFile {
                path: "decoys.js",
                source: "export class arrayWrapper {\n  map() {}\n  filter() {}\n  reduce() {}\n  forEach() {}\n  find() {}\n  findIndex() {}\n  some() {}\n  every() {}\n  push() {}\n  pop() {}\n  slice() {}\n  splice() {}\n  sort() {}\n  flatMap() {}\n  includes() {}\n  join() {}\n}\nexport class collectionWrapper {\n  get() {}\n  set() {}\n  has() {}\n  add() {}\n  delete() {}\n  clear() {}\n  keys() {}\n  values() {}\n  entries() {}\n}\nexport class stringWrapper {\n  trim() {}\n  split() {}\n  replace() {}\n  replaceAll() {}\n  match() {}\n  matchAll() {}\n  search() {}\n  startsWith() {}\n  endsWith() {}\n  substring() {}\n  toLowerCase() {}\n  toUpperCase() {}\n}\nexport class promiseWrapper {\n  then() {}\n  catch() {}\n  finally() {}\n}\nexport class functionWrapper {\n  call() {}\n  apply() {}\n  bind() {}\n}\nexport class eventWrapper {\n  on() {}\n  once() {}\n  off() {}\n  emit() {}\n  addListener() {}\n  removeListener() {}\n  removeAllListeners() {}\n  addEventListener() {}\n  removeEventListener() {}\n  dispatchEvent() {}\n}\nexport class iteratorWrapper {\n  next() {}\n  return() {}\n  throw() {}\n}\n",
            },
            SourceFile {
                path: "calls.js",
                source: "export function array_map(array) { array.map(); }\nexport function array_filter(array) { array.filter(); }\nexport function array_reduce(array) { array.reduce(); }\nexport function array_forEach(array) { array.forEach(); }\nexport function array_find(array) { array.find(); }\nexport function array_findIndex(array) { array.findIndex(); }\nexport function array_some(array) { array.some(); }\nexport function array_every(array) { array.every(); }\nexport function array_push(array) { array.push(); }\nexport function array_pop(array) { array.pop(); }\nexport function array_slice(array) { array.slice(); }\nexport function array_splice(array) { array.splice(); }\nexport function array_sort(array) { array.sort(); }\nexport function array_flatMap(array) { array.flatMap(); }\nexport function array_includes(array) { array.includes(); }\nexport function array_join(array) { array.join(); }\nexport function collection_get(collection) { collection.get(); }\nexport function collection_set(collection) { collection.set(); }\nexport function collection_has(collection) { collection.has(); }\nexport function collection_add(collection) { collection.add(); }\nexport function collection_delete(collection) { collection.delete(); }\nexport function collection_clear(collection) { collection.clear(); }\nexport function collection_keys(collection) { collection.keys(); }\nexport function collection_values(collection) { collection.values(); }\nexport function collection_entries(collection) { collection.entries(); }\nexport function string_trim(string) { string.trim(); }\nexport function string_split(string) { string.split(); }\nexport function string_replace(string) { string.replace(); }\nexport function string_replaceAll(string) { string.replaceAll(); }\nexport function string_match(string) { string.match(); }\nexport function string_matchAll(string) { string.matchAll(); }\nexport function string_search(string) { string.search(); }\nexport function string_startsWith(string) { string.startsWith(); }\nexport function string_endsWith(string) { string.endsWith(); }\nexport function string_substring(string) { string.substring(); }\nexport function string_toLowerCase(string) { string.toLowerCase(); }\nexport function string_toUpperCase(string) { string.toUpperCase(); }\nexport function promise_then(promise) { promise.then(); }\nexport function promise_catch(promise) { promise.catch(); }\nexport function promise_finally(promise) { promise.finally(); }\nexport function function_call(function) { function.call(); }\nexport function function_apply(function) { function.apply(); }\nexport function function_bind(function) { function.bind(); }\nexport function event_on(event) { event.on(); }\nexport function event_once(event) { event.once(); }\nexport function event_off(event) { event.off(); }\nexport function event_emit(event) { event.emit(); }\nexport function event_addListener(event) { event.addListener(); }\nexport function event_removeListener(event) { event.removeListener(); }\nexport function event_removeAllListeners(event) { event.removeAllListeners(); }\nexport function event_addEventListener(event) { event.addEventListener(); }\nexport function event_removeEventListener(event) { event.removeEventListener(); }\nexport function event_dispatchEvent(event) { event.dispatchEvent(); }\nexport function iterator_next(iterator) { iterator.next(); }\nexport function iterator_return(iterator) { iterator.return(); }\nexport function iterator_throw(iterator) { iterator.throw(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "array_map",
                expected: json!([
                    {"name": "array_map", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_filter",
                expected: json!([
                    {"name": "array_filter", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_reduce",
                expected: json!([
                    {"name": "array_reduce", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_forEach",
                expected: json!([
                    {"name": "array_forEach", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_find",
                expected: json!([
                    {"name": "array_find", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_findIndex",
                expected: json!([
                    {"name": "array_findIndex", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_some",
                expected: json!([
                    {"name": "array_some", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_every",
                expected: json!([
                    {"name": "array_every", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_push",
                expected: json!([
                    {"name": "array_push", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_pop",
                expected: json!([
                    {"name": "array_pop", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_slice",
                expected: json!([
                    {"name": "array_slice", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_splice",
                expected: json!([
                    {"name": "array_splice", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_sort",
                expected: json!([
                    {"name": "array_sort", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_flatMap",
                expected: json!([
                    {"name": "array_flatMap", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_includes",
                expected: json!([
                    {"name": "array_includes", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_join",
                expected: json!([
                    {"name": "array_join", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_get",
                expected: json!([
                    {"name": "collection_get", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_set",
                expected: json!([
                    {"name": "collection_set", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_has",
                expected: json!([
                    {"name": "collection_has", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_add",
                expected: json!([
                    {"name": "collection_add", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_delete",
                expected: json!([
                    {"name": "collection_delete", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_clear",
                expected: json!([
                    {"name": "collection_clear", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_keys",
                expected: json!([
                    {"name": "collection_keys", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_values",
                expected: json!([
                    {"name": "collection_values", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_entries",
                expected: json!([
                    {"name": "collection_entries", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_trim",
                expected: json!([
                    {"name": "string_trim", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_split",
                expected: json!([
                    {"name": "string_split", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_replace",
                expected: json!([
                    {"name": "string_replace", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_replaceAll",
                expected: json!([
                    {"name": "string_replaceAll", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_match",
                expected: json!([
                    {"name": "string_match", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_matchAll",
                expected: json!([
                    {"name": "string_matchAll", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_search",
                expected: json!([
                    {"name": "string_search", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_startsWith",
                expected: json!([
                    {"name": "string_startsWith", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_endsWith",
                expected: json!([
                    {"name": "string_endsWith", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_substring",
                expected: json!([
                    {"name": "string_substring", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_toLowerCase",
                expected: json!([
                    {"name": "string_toLowerCase", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_toUpperCase",
                expected: json!([
                    {"name": "string_toUpperCase", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "promise_then",
                expected: json!([
                    {"name": "promise_then", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "promise_catch",
                expected: json!([
                    {"name": "promise_catch", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "promise_finally",
                expected: json!([
                    {"name": "promise_finally", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "function_call",
                expected: json!([
                    {"name": "function_call", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "function_apply",
                expected: json!([
                    {"name": "function_apply", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "function_bind",
                expected: json!([
                    {"name": "function_bind", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_on",
                expected: json!([
                    {"name": "event_on", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_once",
                expected: json!([
                    {"name": "event_once", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_off",
                expected: json!([
                    {"name": "event_off", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_emit",
                expected: json!([
                    {"name": "event_emit", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_addListener",
                expected: json!([
                    {"name": "event_addListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_removeListener",
                expected: json!([
                    {"name": "event_removeListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_removeAllListeners",
                expected: json!([
                    {"name": "event_removeAllListeners", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_addEventListener",
                expected: json!([
                    {"name": "event_addEventListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_removeEventListener",
                expected: json!([
                    {"name": "event_removeEventListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_dispatchEvent",
                expected: json!([
                    {"name": "event_dispatchEvent", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "iterator_next",
                expected: json!([
                    {"name": "iterator_next", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "iterator_return",
                expected: json!([
                    {"name": "iterator_return", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "iterator_throw",
                expected: json!([
                    {"name": "iterator_throw", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// js-builtin-method-calls.test.ts:68 — rejects built-in values but retains constructed, imported, literal and class receivers
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn js_builtin_method_calls_68_js() {
    check_case(Case {
        name: "js_builtin_method_calls_68_js",
        files: vec![
            SourceFile {
                path: "cart.js",
                source: "\nexport class Cart {\n  map() {}\n  add() {}\n  get() {}\n  static bind() {}\n  self() { this.add(); }\n}\nexport const api = { map() {} };\nexport class LRUCache { get() {} }\n",
            },
            SourceFile {
                path: "calls.js",
                source: "\nimport { Cart, api } from './cart';\nexport function tidy(list) { list.map(); }\nexport function cached() {\n  const cache = new Map();\n  cache.get('x');\n}\nexport function unknownCache(cache) { cache.get('x'); }\nexport function literalArray() { const cart = []; cart.map(); }\nexport function literalString() { const cart = 'x'; cart.trim(); }\nexport function constructed() {\n  const cart = new Cart();\n  cart.add();\n}\nexport function imported() { Cart.bind(); }\nexport function objectLiteral() { api.map(); }\nexport const local = { map() {} };\nexport function localLiteral() { local.map(); }\n\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "tidy",
                expected: json!([
                    {"name": "tidy", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "cached",
                expected: json!([
                    {"name": "cached", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "unknownCache",
                expected: json!([
                    {"name": "unknownCache", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "literalArray",
                expected: json!([
                    {"name": "literalArray", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "literalString",
                expected: json!([
                    {"name": "literalString", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "constructed",
                expected: json!([
                    {"name": "constructed", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "imported",
                expected: json!([
                    {"name": "imported", "edges": [
                        {"symbol": {"name": "Cart::bind"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "objectLiteral",
                expected: json!([
                    {"name": "objectLiteral", "edges": [
                        {"symbol": {"name": "api::map"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "localLiteral",
                expected: json!([
                    {"name": "localLiteral", "edges": [
                        {"symbol": {"name": "local::map"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Cart::self",
                expected: json!([
                    {"name": "Cart::self", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// js-builtin-method-calls.test.ts:26 — does not guess project methods for unknown receivers, including class-name overlap
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn js_builtin_method_calls_26_jsx() {
    check_case(Case {
        name: "js_builtin_method_calls_26_jsx",
        files: vec![
            SourceFile {
                path: "decoys.jsx",
                source: "export class arrayWrapper {\n  map() {}\n  filter() {}\n  reduce() {}\n  forEach() {}\n  find() {}\n  findIndex() {}\n  some() {}\n  every() {}\n  push() {}\n  pop() {}\n  slice() {}\n  splice() {}\n  sort() {}\n  flatMap() {}\n  includes() {}\n  join() {}\n}\nexport class collectionWrapper {\n  get() {}\n  set() {}\n  has() {}\n  add() {}\n  delete() {}\n  clear() {}\n  keys() {}\n  values() {}\n  entries() {}\n}\nexport class stringWrapper {\n  trim() {}\n  split() {}\n  replace() {}\n  replaceAll() {}\n  match() {}\n  matchAll() {}\n  search() {}\n  startsWith() {}\n  endsWith() {}\n  substring() {}\n  toLowerCase() {}\n  toUpperCase() {}\n}\nexport class promiseWrapper {\n  then() {}\n  catch() {}\n  finally() {}\n}\nexport class functionWrapper {\n  call() {}\n  apply() {}\n  bind() {}\n}\nexport class eventWrapper {\n  on() {}\n  once() {}\n  off() {}\n  emit() {}\n  addListener() {}\n  removeListener() {}\n  removeAllListeners() {}\n  addEventListener() {}\n  removeEventListener() {}\n  dispatchEvent() {}\n}\nexport class iteratorWrapper {\n  next() {}\n  return() {}\n  throw() {}\n}\n",
            },
            SourceFile {
                path: "calls.jsx",
                source: "export function array_map(array) { array.map(); }\nexport function array_filter(array) { array.filter(); }\nexport function array_reduce(array) { array.reduce(); }\nexport function array_forEach(array) { array.forEach(); }\nexport function array_find(array) { array.find(); }\nexport function array_findIndex(array) { array.findIndex(); }\nexport function array_some(array) { array.some(); }\nexport function array_every(array) { array.every(); }\nexport function array_push(array) { array.push(); }\nexport function array_pop(array) { array.pop(); }\nexport function array_slice(array) { array.slice(); }\nexport function array_splice(array) { array.splice(); }\nexport function array_sort(array) { array.sort(); }\nexport function array_flatMap(array) { array.flatMap(); }\nexport function array_includes(array) { array.includes(); }\nexport function array_join(array) { array.join(); }\nexport function collection_get(collection) { collection.get(); }\nexport function collection_set(collection) { collection.set(); }\nexport function collection_has(collection) { collection.has(); }\nexport function collection_add(collection) { collection.add(); }\nexport function collection_delete(collection) { collection.delete(); }\nexport function collection_clear(collection) { collection.clear(); }\nexport function collection_keys(collection) { collection.keys(); }\nexport function collection_values(collection) { collection.values(); }\nexport function collection_entries(collection) { collection.entries(); }\nexport function string_trim(string) { string.trim(); }\nexport function string_split(string) { string.split(); }\nexport function string_replace(string) { string.replace(); }\nexport function string_replaceAll(string) { string.replaceAll(); }\nexport function string_match(string) { string.match(); }\nexport function string_matchAll(string) { string.matchAll(); }\nexport function string_search(string) { string.search(); }\nexport function string_startsWith(string) { string.startsWith(); }\nexport function string_endsWith(string) { string.endsWith(); }\nexport function string_substring(string) { string.substring(); }\nexport function string_toLowerCase(string) { string.toLowerCase(); }\nexport function string_toUpperCase(string) { string.toUpperCase(); }\nexport function promise_then(promise) { promise.then(); }\nexport function promise_catch(promise) { promise.catch(); }\nexport function promise_finally(promise) { promise.finally(); }\nexport function function_call(function) { function.call(); }\nexport function function_apply(function) { function.apply(); }\nexport function function_bind(function) { function.bind(); }\nexport function event_on(event) { event.on(); }\nexport function event_once(event) { event.once(); }\nexport function event_off(event) { event.off(); }\nexport function event_emit(event) { event.emit(); }\nexport function event_addListener(event) { event.addListener(); }\nexport function event_removeListener(event) { event.removeListener(); }\nexport function event_removeAllListeners(event) { event.removeAllListeners(); }\nexport function event_addEventListener(event) { event.addEventListener(); }\nexport function event_removeEventListener(event) { event.removeEventListener(); }\nexport function event_dispatchEvent(event) { event.dispatchEvent(); }\nexport function iterator_next(iterator) { iterator.next(); }\nexport function iterator_return(iterator) { iterator.return(); }\nexport function iterator_throw(iterator) { iterator.throw(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "array_map",
                expected: json!([
                    {"name": "array_map", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_filter",
                expected: json!([
                    {"name": "array_filter", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_reduce",
                expected: json!([
                    {"name": "array_reduce", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_forEach",
                expected: json!([
                    {"name": "array_forEach", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_find",
                expected: json!([
                    {"name": "array_find", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_findIndex",
                expected: json!([
                    {"name": "array_findIndex", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_some",
                expected: json!([
                    {"name": "array_some", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_every",
                expected: json!([
                    {"name": "array_every", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_push",
                expected: json!([
                    {"name": "array_push", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_pop",
                expected: json!([
                    {"name": "array_pop", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_slice",
                expected: json!([
                    {"name": "array_slice", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_splice",
                expected: json!([
                    {"name": "array_splice", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_sort",
                expected: json!([
                    {"name": "array_sort", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_flatMap",
                expected: json!([
                    {"name": "array_flatMap", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_includes",
                expected: json!([
                    {"name": "array_includes", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "array_join",
                expected: json!([
                    {"name": "array_join", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_get",
                expected: json!([
                    {"name": "collection_get", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_set",
                expected: json!([
                    {"name": "collection_set", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_has",
                expected: json!([
                    {"name": "collection_has", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_add",
                expected: json!([
                    {"name": "collection_add", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_delete",
                expected: json!([
                    {"name": "collection_delete", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_clear",
                expected: json!([
                    {"name": "collection_clear", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_keys",
                expected: json!([
                    {"name": "collection_keys", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_values",
                expected: json!([
                    {"name": "collection_values", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "collection_entries",
                expected: json!([
                    {"name": "collection_entries", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_trim",
                expected: json!([
                    {"name": "string_trim", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_split",
                expected: json!([
                    {"name": "string_split", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_replace",
                expected: json!([
                    {"name": "string_replace", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_replaceAll",
                expected: json!([
                    {"name": "string_replaceAll", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_match",
                expected: json!([
                    {"name": "string_match", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_matchAll",
                expected: json!([
                    {"name": "string_matchAll", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_search",
                expected: json!([
                    {"name": "string_search", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_startsWith",
                expected: json!([
                    {"name": "string_startsWith", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_endsWith",
                expected: json!([
                    {"name": "string_endsWith", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_substring",
                expected: json!([
                    {"name": "string_substring", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_toLowerCase",
                expected: json!([
                    {"name": "string_toLowerCase", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "string_toUpperCase",
                expected: json!([
                    {"name": "string_toUpperCase", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "promise_then",
                expected: json!([
                    {"name": "promise_then", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "promise_catch",
                expected: json!([
                    {"name": "promise_catch", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "promise_finally",
                expected: json!([
                    {"name": "promise_finally", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "function_call",
                expected: json!([
                    {"name": "function_call", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "function_apply",
                expected: json!([
                    {"name": "function_apply", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "function_bind",
                expected: json!([
                    {"name": "function_bind", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_on",
                expected: json!([
                    {"name": "event_on", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_once",
                expected: json!([
                    {"name": "event_once", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_off",
                expected: json!([
                    {"name": "event_off", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_emit",
                expected: json!([
                    {"name": "event_emit", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_addListener",
                expected: json!([
                    {"name": "event_addListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_removeListener",
                expected: json!([
                    {"name": "event_removeListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_removeAllListeners",
                expected: json!([
                    {"name": "event_removeAllListeners", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_addEventListener",
                expected: json!([
                    {"name": "event_addEventListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_removeEventListener",
                expected: json!([
                    {"name": "event_removeEventListener", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "event_dispatchEvent",
                expected: json!([
                    {"name": "event_dispatchEvent", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "iterator_next",
                expected: json!([
                    {"name": "iterator_next", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "iterator_return",
                expected: json!([
                    {"name": "iterator_return", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "iterator_throw",
                expected: json!([
                    {"name": "iterator_throw", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// js-builtin-method-calls.test.ts:68 — rejects built-in values but retains constructed, imported, literal and class receivers
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn js_builtin_method_calls_68_jsx() {
    check_case(Case {
        name: "js_builtin_method_calls_68_jsx",
        files: vec![
            SourceFile {
                path: "cart.jsx",
                source: "\nexport class Cart {\n  map() {}\n  add() {}\n  get() {}\n  static bind() {}\n  self() { this.add(); }\n}\nexport const api = { map() {} };\nexport class LRUCache { get() {} }\n",
            },
            SourceFile {
                path: "calls.jsx",
                source: "\nimport { Cart, api } from './cart';\nexport function tidy(list) { list.map(); }\nexport function cached() {\n  const cache = new Map();\n  cache.get('x');\n}\nexport function unknownCache(cache) { cache.get('x'); }\nexport function literalArray() { const cart = []; cart.map(); }\nexport function literalString() { const cart = 'x'; cart.trim(); }\nexport function constructed() {\n  const cart = new Cart();\n  cart.add();\n}\nexport function imported() { Cart.bind(); }\nexport function objectLiteral() { api.map(); }\nexport const local = { map() {} };\nexport function localLiteral() { local.map(); }\n\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "tidy",
                expected: json!([
                    {"name": "tidy", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "cached",
                expected: json!([
                    {"name": "cached", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "unknownCache",
                expected: json!([
                    {"name": "unknownCache", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "literalArray",
                expected: json!([
                    {"name": "literalArray", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "literalString",
                expected: json!([
                    {"name": "literalString", "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "constructed",
                expected: json!([
                    {"name": "constructed", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "imported",
                expected: json!([
                    {"name": "imported", "edges": [
                        {"symbol": {"name": "Cart::bind"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "objectLiteral",
                expected: json!([
                    {"name": "objectLiteral", "edges": [
                        {"symbol": {"name": "api::map"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "localLiteral",
                expected: json!([
                    {"name": "localLiteral", "edges": [
                        {"symbol": {"name": "local::map"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Cart::self",
                expected: json!([
                    {"name": "Cart::self", "edges": [
                        {"symbol": {"name": "Cart::add"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:1458
#[tokio::test]
async fn resolution_1458() {
    check_case(Case {
        name: "resolution_1458",
        files: vec![
            SourceFile {
                path: "helpers.ts",
                source: "export class Foo {\n  static bar(x: number) { return x + 1; }\n}\n",
            },
            SourceFile {
                path: "caller.ts",
                source: "import { Foo } from './helpers';\nexport function run() { return Foo.bar(41); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "bar",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "run"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2498
#[tokio::test]
async fn resolution_2498() {
    check_case(Case {
        name: "resolution_2498",
        files: vec![
            SourceFile {
                path: "strings.ts",
                source: "\nexport async function listPaths(): Promise<string> { return \"a\u{0000}b\"; }\nexport async function snapshot(): Promise<string[]> {\n  const listed = await listPaths();\n  return listed.split('\u{0000}');\n}\n",
            },
            SourceFile {
                path: "pane.ts",
                source: "\nexport class PaneManager {\n  split(): string { return \"new pane\"; }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "snapshot",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "listPaths"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2524
#[tokio::test]
async fn resolution_2524() {
    check_case(Case {
        name: "resolution_2524",
        files: vec![
            SourceFile {
                path: "engine.ts",
                source: "\nexport class Engine {\n  run(): string { return \"ran\"; }\n}\nexport class Decoy {\n  run(): string { return \"decoy\"; }\n}\nexport async function makeEngine(): Promise<Engine> { return new Engine(); }\nexport async function drive(): Promise<string> {\n  const handle = await makeEngine();\n  return handle.run();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "drive",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Engine::run"}},
                        {"symbol": {"name": "makeEngine"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2551
// Disabled: the expectation omits new Map(); constructor-call semantics differ.
// #[tokio::test]
// async fn resolution_2551() {
//     check_case(Case {
//         name: "resolution_2551",
//         files: vec![
//             SourceFile {
//                 path: "shadow.ts",
//                 source: "\nexport class Map { get() { return 1; } }\nexport class Other { get() { return 2; } }\nexport function useShadow() {\n  const values = new Map();\n  return values.get();\n}\n",
//             },
//         ],
//         queries: vec![
//             Query {
//                 operation: "callees",
//                 symbol: "useShadow",
//                 expected: json!([
//                     {"edges": [
//                         {"symbol": {"name": "Map::get"}},
//                     ]},
//                 ]),
//                 ..Query::default()
//             },
//         ],
//     }).await;
// }

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2674
#[tokio::test]
async fn resolution_2674() {
    check_case(Case {
        name: "resolution_2674",
        files: vec![
            SourceFile {
                path: "svc.ts",
                source: "class Logger { log() { return 1; } }\nclass Other { log() { return 2; } }\nexport function use(lg: Logger) { return lg.log(); }\nexport function useOther(o: Other) { return o.log(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "Logger::log",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "use"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Logger::log",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "useOther"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Other::log",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "useOther"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Other::log",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "use"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2890
#[tokio::test]
async fn resolution_2890() {
    check_case(Case {
        name: "resolution_2890",
        files: vec![
            SourceFile {
                path: "src/utils/format.ts",
                source: "export function pickMe(): number { return 1; }\n",
            },
            SourceFile {
                path: "src/legacy/format.ts",
                source: "export function pickMe(): number { return 99; }\n",
            },
            SourceFile {
                path: "src/main.ts",
                source: "import { pickMe } from '@utils/format';\nexport function go(): number { return pickMe(); }\n",
            },
            SourceFile {
                path: "tsconfig.json",
                source: "{\"compilerOptions\":{\"baseUrl\":\"./src\",\"paths\":{\"@utils/*\":[\"utils/*\"]}}}",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "pickMe",
                expected: json!([
                    {"filePath": "src/utils/format.ts", "edges": [
                        {},
                    ]},
                ]),
                file: Some("src/utils/format.ts"),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "pickMe",
                expected: json!([
                    {"filePath": "src/utils/format.ts", "edges": [
                        {"symbol": {"filePath": "src/main.ts"}},
                    ]},
                ]),
                file: Some("src/utils/format.ts"),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "pickMe",
                expected: json!([
                    {"filePath": "src/legacy/format.ts", "edges": [
                        {"symbol": {"filePath": "src/main.ts"}},
                    ]},
                ]),
                file: Some("src/legacy/format.ts"),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2939
#[tokio::test]
async fn resolution_2939() {
    check_case(Case {
        name: "resolution_2939",
        files: vec![
            SourceFile {
                path: "src/a.ts",
                source: "export function aFn(): void {}\n",
            },
            SourceFile {
                path: "src/b.ts",
                source: "import { aFn } from './a';\nexport function bFn(): void { aFn(); }\n",
            },
        ],
        queries: vec![Query {
            operation: "callers",
            symbol: "aFn",
            expected: json!([
                {"edges": [
                    {"symbol": {"filePath": "src/b.ts"}},
                ]},
            ]),
            check: Check::Contains,
            ..Query::default()
        }],
    })
    .await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2961
#[tokio::test]
async fn resolution_2961() {
    check_case(Case {
        name: "resolution_2961",
        files: vec![
            SourceFile {
                path: "src/services/auth.ts",
                source: "export function signIn(): void {}\n",
            },
            SourceFile {
                path: "src/services/index.ts",
                source: "export { signIn } from './auth';\n",
            },
            SourceFile {
                path: "src/all.ts",
                source: "export * from './services/index';\n",
            },
            SourceFile {
                path: "src/main.ts",
                source: "import { signIn } from './all';\nexport function go(): void { signIn(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "signIn",
                expected: json!([
                    {"filePath": "src/services/auth.ts", "edges": [
                        {"symbol": {"filePath": "src/main.ts"}},
                    ]},
                ]),
                file: Some("src/services/auth.ts"),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2994
#[tokio::test]
async fn resolution_2994() {
    check_case(Case {
        name: "resolution_2994",
        files: vec![
            SourceFile {
                path: "src/auth.ts",
                source: "export function signIn(): void {}\n",
            },
            SourceFile {
                path: "src/index.ts",
                source: "export { signIn as login } from './auth';\n",
            },
            SourceFile {
                path: "src/main.ts",
                source: "import { login } from './index';\nexport function go(): void { login(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "signIn",
                expected: json!([
                    {"filePath": "src/auth.ts", "edges": [
                        {"symbol": {"filePath": "src/main.ts"}},
                    ]},
                ]),
                file: Some("src/auth.ts"),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:3099
#[tokio::test]
async fn resolution_3099() {
    check_case(Case {
        name: "resolution_3099",
        files: vec![
            SourceFile {
                path: "src/util.ts",
                source: "export function realHelper(): void {}\n",
            },
            SourceFile {
                path: "src/index.ts",
                source: "export { realHelper as helper } from './util';\n",
            },
            SourceFile {
                path: "src/main.ts",
                source: "import { helper } from '.';\nexport function go(): void { helper(); }\n",
            },
            SourceFile {
                path: "src/main2.ts",
                source: "import { helper } from './';\nexport function go2(): void { helper(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "realHelper",
                expected: json!([
                    {"filePath": "src/util.ts", "edges": [
                        {"symbol": {"filePath": "src/main.ts"}},
                    ]},
                ]),
                file: Some("src/util.ts"),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "realHelper",
                expected: json!([
                    {"filePath": "src/util.ts", "edges": [
                        {"symbol": {"filePath": "src/main2.ts"}},
                    ]},
                ]),
                file: Some("src/util.ts"),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    })
    .await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:3243
#[tokio::test]
async fn resolution_3243() {
    check_case(Case {
        name: "resolution_3243",
        files: vec![
            SourceFile {
                path: "repro.py",
                source: "def format_fields(values):\n    def join(vals):\n        return \"-\".join(sorted(vals))\n\n    return join(values)\n\n\ndef report_missing(unresolved):\n    missing_list = \", \".join(sorted(unresolved))\n    return f\"Could not resolve: {missing_list}\"\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "join",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "format_fields"}},
                    ]},
                ]),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "report_missing",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                endpoint_suffix: Some("join"),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:3295
#[tokio::test]
async fn resolution_3295() {
    check_case(Case {
        name: "resolution_3295",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: "module example.com/app\n\ngo 1.22\n",
            },
            SourceFile {
                path: "flow/flow.go",
                source: "package flow\n\nimport \"database/sql\"\n\ntype InternalStore interface {\n\tExec(string, ...any) (sql.Result, error)\n\tQueryRow(string, ...any) *sql.Row\n}\n\ntype Target struct{ conn *sql.DB }\n\nfunc (target *Target) Write() error {\n\t_, err := target.conn.Exec(\"insert\")\n\treturn err\n}\n\nfunc (target *Target) Read() *sql.Row {\n\treturn target.conn.QueryRow(\"select\")\n}\n\ntype Store struct{}\n\nfunc (s *Store) Put(key string) {}\n\ntype Repo struct{ db *Store }\n\nfunc (r *Repo) Save() {\n\tr.db.Put(\"k\")\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "Exec",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Write"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "QueryRow",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Read"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Put",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Save"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:3368
#[tokio::test]
async fn resolution_3368() {
    check_case(Case {
        name: "resolution_3368",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: "module example.com/b\n\ngo 1.22\n",
            },
            SourceFile {
                path: "m.go",
                source: "package m\n\nimport \"net/http\"\n\ntype node struct{}\n\nfunc (n *node) InsertRoute(path string) {}\n\n// A local type sharing the stdlib interface's name — the decoy the\n// package-qualifier gate exists for.\ntype Handler func()\n\nfunc (h Handler) ServeHTTP() {}\n\ntype Mux struct {\n\t// the tree router lives below this comment (the comment must not\n\t// donate a field type)\n\thandler http.Handler\n\ttree    *node\n}\n\nfunc (mx *Mux) handle(path string) {\n\tmx.tree.InsertRoute(path)\n}\n\nfunc (mx *Mux) dispatch() {\n\tmx.handler.ServeHTTP(nil, nil)\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "InsertRoute",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "handle"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "ServeHTTP",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "dispatch"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:3439
#[tokio::test]
async fn resolution_3439() {
    check_case(Case {
        name: "resolution_3439",
        files: vec![
            SourceFile {
                path: "src/store.ts",
                source: "export class ReproStore {\n  notifyJoinGuildStatus(): void {\n    console.log('notified');\n  }\n}\n\nexport const reproStore = new ReproStore();\n\nexport function callInDefinitionFile(): void {\n  reproStore.notifyJoinGuildStatus();\n}\n",
            },
            SourceFile {
                path: "src/caller.ts",
                source: "import { reproStore } from './store';\n\nexport function callFromImportedFile(): void {\n  reproStore.notifyJoinGuildStatus();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "notifyJoinGuildStatus",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "callInDefinitionFile"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "notifyJoinGuildStatus",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "callFromImportedFile"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// frameworks-integration.test.ts:216
#[tokio::test]
async fn frameworks_integration_216() {
    check_case(Case {
        name: "frameworks_integration_216",
        files: vec![
            SourceFile {
                path: "detect.hpp",
                source: "class CDetect {\n public:\n  int Processing();\n};\nclass CDetector {\n private:\n  CDetect* m_cpAlg = nullptr;\n public:\n  int Run();\n  int Flush();\n};\n",
            },
            SourceFile {
                path: "detect.cpp",
                source: "#include \"detect.hpp\"\nint CDetector::Run() { return m_cpAlg->Processing(); }\nint CDetector::Flush() { return m_cpAlg->Processing(); }\nint CDetect::Processing() { return 0; }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "CDetect::Processing",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "CDetector::Run"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "CDetect::Processing",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "CDetector::Flush"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "CDetector::Run",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "CDetect::Processing"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// frameworks-integration.test.ts:265
#[tokio::test]
async fn frameworks_integration_265() {
    check_case(Case {
        name: "frameworks_integration_265",
        files: vec![
            SourceFile {
                path: "detect.hpp",
                source: "class CDetect { public: int Processing(); };\nclass CWidget { public: int Processing(); };\nclass CDetector {\n private:\n  CDetect* m_cpAlg = nullptr;\n public:\n  int RunReturn();\n  int RunAssign();\n};\n",
            },
            SourceFile {
                path: "detect.cpp",
                source: "#include \"detect.hpp\"\nint CDetector::RunReturn() { return m_cpAlg->Processing(); }\nint CDetector::RunAssign() { int r = m_cpAlg->Processing(); return r; }\nint CDetect::Processing() { return 0; }\nint CWidget::Processing() { return 0; }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "CDetect::Processing",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "CDetector::RunReturn"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "CDetect::Processing",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "CDetector::RunAssign"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// frameworks-integration.test.ts:357
#[tokio::test]
async fn frameworks_integration_357() {
    check_case(Case {
        name: "frameworks_integration_357",
        files: vec![
            SourceFile {
                path: "store.cc",
                source: "class Store {\npublic:\n    virtual ~Store() {}\n    virtual int read(int key) = 0;\n};\nclass DiskStore : public Store {\npublic:\n    int read(int key) override { return key + 1; }\n};\nclass MemStore : public Store {\npublic:\n    int read(int key) override { return key + 2; }\n};\nint fetch(Store* s, int k) {\n    return s->read(k);\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "fetch",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Store::read"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// function-ref.test.ts:863
// Disabled: a Base-typed field does not establish Store as its runtime target.
// #[tokio::test]
// async fn function_ref_863() {
//     check_case(Case {
//         name: "function_ref_863",
//         files: vec![
//             SourceFile {
//                 path: "store.py",
//                 source: "class Base:\n    pass\n\nclass Store(Base):\n    def fetch(self, ids):\n        return ids",
//             },
//             SourceFile {
//                 path: "consumer.py",
//                 source: "from concurrent.futures import ThreadPoolExecutor\nfrom store import Base\n\nclass Consumer:\n    def __init__(self, store: Base):\n        self.store = store\n\n    def direct(self, ids):\n        return self.store.fetch(ids)\n\n    def via_callback(self, ids, pool: ThreadPoolExecutor):\n        return pool.submit(self.store.fetch, ids)",
//             },
//         ],
//         queries: vec![
//             Query {
//                 operation: "callers",
//                 symbol: "fetch",
//                 expected: json!([
//                     {"edges": [
//                         {"symbol": {"name": "direct"}},
//                     ]},
//                 ]),
//                 short_names: true,
//                 check: Check::Contains,
//                 ..Query::default()
//             },
//         ],
//     }).await;
// }

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// function-ref.test.ts:1012
#[tokio::test]
async fn function_ref_1012() {
    check_case(Case {
        name: "function_ref_1012",
        files: vec![
            SourceFile {
                path: "store.go",
                source: "package demo\n\ntype Store struct{}\n\nfunc (s *Store) Fetch(ids []string) []string { return ids }",
            },
            SourceFile {
                path: "consumer.go",
                source: "package demo\n\nfunc Submit(fn func([]string) []string, ids []string) []string { return fn(ids) }\n\ntype Consumer struct{ store *Store }\n\nfunc (c *Consumer) Direct(ids []string) []string { return c.store.Fetch(ids) }\n\nfunc (c *Consumer) ViaGo(ids []string) { go c.store.Fetch(ids) }\n\nfunc (c *Consumer) ViaSubmit(ids []string) []string { return Submit(c.store.Fetch, ids) }",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "Fetch",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Direct"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Fetch",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "ViaGo"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// object-literal-methods.test.ts:137
#[tokio::test]
async fn object_literal_methods_137() {
    check_case(Case {
        name: "object_literal_methods_137",
        files: vec![
            SourceFile {
                path: "package.json",
                source: "{\"name\":\"t\",\"dependencies\":{\"zustand\":\"^4\"}}\n",
            },
            SourceFile {
                path: "store.ts",
                source: "import { create } from 'zustand'\ninterface S { fetchUser(): Promise<void>; reset(): void }\nexport const useStore = create<S>((set, get) => ({\n  fetchUser: async () => { get().reset() },\n  reset: () => set({}),\n}))\n",
            },
            SourceFile {
                path: "caller.ts",
                source: "import { useStore } from './store'\nexport async function loginFlow() {\n  const { fetchUser } = useStore.getState()\n  await fetchUser()\n}\nexport function hardReset() {\n  useStore.getState().reset()\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "fetchUser",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "loginFlow"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "reset",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "hardReset"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "reset",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "fetchUser"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2633 — `resolves a local-variable method call — ${c.lang}`
#[tokio::test]
async fn resolution_2633_ts() {
    check_case(Case {
        name: "resolution_2633_ts",
        files: vec![
            SourceFile {
                path: "svc.ts",
                source: "class Logger { log() { return 1; } }\nexport function use() { const lg = new Logger(); return lg.log(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "log",
                expected: json!([
                    {"edges": [
                        {},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2633 — `resolves a local-variable method call — ${c.lang}`
#[tokio::test]
async fn resolution_2633_js() {
    check_case(Case {
        name: "resolution_2633_js",
        files: vec![
            SourceFile {
                path: "svc.js",
                source: "class Logger { log() { return 1; } }\nexport function use() { const lg = new Logger(); return lg.log(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "log",
                expected: json!([
                    {"edges": [
                        {},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2633 — `resolves a local-variable method call — ${c.lang}`
#[tokio::test]
async fn resolution_2633_py() {
    check_case(Case {
        name: "resolution_2633_py",
        files: vec![
            SourceFile {
                path: "svc.py",
                source: "class Logger:\n    def log(self):\n        return 1\ndef use():\n    lg = Logger()\n    return lg.log()\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "log",
                expected: json!([
                    {"edges": [
                        {},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2633 — `resolves a local-variable method call — ${c.lang}`
#[tokio::test]
async fn resolution_2633_java() {
    check_case(Case {
        name: "resolution_2633_java",
        files: vec![
            SourceFile {
                path: "Svc.java",
                source: "class Logger { void log() { int a = 1; } }\nclass Use { void run() { Logger lg = new Logger(); lg.log(); } }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "log",
                expected: json!([
                    {"edges": [
                        {},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2633 — `resolves a local-variable method call — ${c.lang}`
#[tokio::test]
async fn resolution_2633_go() {
    check_case(Case {
        name: "resolution_2633_go",
        files: vec![
            SourceFile {
                path: "svc.go",
                source: "package a\ntype Logger struct{}\nfunc (l Logger) Log() int { return 1 }\nfunc Use() int { lg := Logger{}; return lg.Log() }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "Log",
                expected: json!([
                    {"edges": [
                        {},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2633 — `resolves a local-variable method call — ${c.lang}`
#[tokio::test]
async fn resolution_2633_rs() {
    check_case(Case {
        name: "resolution_2633_rs",
        files: vec![
            SourceFile {
                path: "svc.rs",
                source: "pub struct Logger { n: i32 }\nimpl Logger { pub fn log(&self) -> i32 { self.n } }\npub fn use_it() -> i32 { let lg = Logger { n: 1 }; lg.log() }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "log",
                expected: json!([
                    {"edges": [
                        {},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2737 — `infers a typed-parameter receiver, disambiguating same-named methods — ${c.lang} (#1125)`
#[tokio::test]
async fn resolution_2737_rs() {
    check_case(Case {
        name: "resolution_2737_rs",
        files: vec![
            SourceFile {
                path: "svc.rs",
                source: "pub struct Logger { n: i32 }\nimpl Logger { pub fn log(&self) -> i32 { self.n } }\npub struct Other { n: i32 }\nimpl Other { pub fn log(&self) -> i32 { self.n } }\npub fn use_it(lg: &Logger) -> i32 { lg.log() }\npub fn use_other(o: &Other) -> i32 { o.log() }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "Logger::log",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "use_it"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Logger::log",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "use_other"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Other::log",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "use_other"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Other::log",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "use_it"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:2737 — `infers a typed-parameter receiver, disambiguating same-named methods — ${c.lang} (#1125)`
#[tokio::test]
async fn resolution_2737_go() {
    check_case(Case {
        name: "resolution_2737_go",
        files: vec![
            SourceFile {
                path: "svc.go",
                source: "package a\ntype Logger struct{}\nfunc (l Logger) Log() int { return 1 }\ntype Other struct{}\nfunc (o Other) Log() int { return 2 }\nfunc UseIt(lg Logger) int { return lg.Log() }\nfunc UseOther(o Other) int { return o.Log() }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "Logger::Log",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "UseIt"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Logger::Log",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "UseOther"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Other::Log",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "UseOther"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Other::Log",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "UseIt"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:3509 — resolves same-file and imported calls to the literal member, never to the constant (#1573)
#[tokio::test]
async fn resolution_3509() {
    check_case(Case {
        name: "resolution_3509",
        files: vec![
            SourceFile {
                path: "a.ts",
                source: "export const obj = { m() { return 1; } };\nexport class C { static s() { return 2; } }\nexport function sameFileCallers() { return obj.m() + C.s(); }\n",
            },
            SourceFile {
                path: "b.ts",
                source: "import { obj, C } from \"./a\";\nexport function crossFileCaller() { return obj.m() + C.s(); }\n",
            },
            SourceFile {
                path: "decoy.ts",
                source: "export function m() { return 'decoy'; }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "m",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "crossFileCaller"}},
                        {"symbol": {"name": "sameFileCallers"}},
                    ]},
                ]),
                file: Some("a.ts"),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "m",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                file: Some("decoy.ts"),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "C::s",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "crossFileCaller"}},
                        {"symbol": {"name": "sameFileCallers"}},
                    ]},
                ]),
                short_names: true,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:3545 — covers method and arrow-property members, and skips a declaration nested in a member body
#[tokio::test]
async fn resolution_3545() {
    check_case(Case {
        name: "resolution_3545",
        files: vec![
            SourceFile {
                path: "src/api.ts",
                source: "export const api = {\n  call: () => { return 1; },\n  get() {\n    function call() { return 'nested in get, not a member'; }\n    return call();\n  },\n};\n",
            },
            SourceFile {
                path: "src/use.ts",
                source: "import { api } from './api';\nexport function useCall() { return api.call(); }\nexport function useGet() { return api.get(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "call",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "useCall"}},
                    ]},
                ]),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "call",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "useCall"}},
                    ]},
                ]),
                short_names: true,
                definition_line: Some(4),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "get",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "useGet"}},
                    ]},
                ]),
                short_names: true,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// import-type-modifier.test.ts:82 — `items$.getState().inc()` reaches the store action unless a local or parameter shadows `items$`
#[tokio::test]
async fn import_type_modifier_82() {
    check_case(Case {
        name: "import_type_modifier_82",
        files: vec![
            SourceFile {
                path: "package.json",
                source: "{\"name\":\"shop\",\"dependencies\":{\"zustand\":\"*\"}}",
            },
            SourceFile {
                path: "src/store.ts",
                source: "import { create } from 'zustand';\n\nconst items$ = create((set) => ({ inc: () => set({}) }));\nexport default items$;\n",
            },
            SourceFile {
                path: "src/util.ts",
                source: "export function helper() { return 1; }\n",
            },
            SourceFile {
                path: "src/decoy.ts",
                source: "export function helper() { return 2; }\n",
            },
            SourceFile {
                path: "src/use.ts",
                source: "import items$ from './store';\nimport * as util$ from './util';\n\nexport function clickInc() { items$.getState().inc(); }\nexport function shadowed() { const items$ = other(); items$.getState().inc(); }\nexport function param(items$: Store) { items$.getState().inc(); }\nexport function run() { return util$.helper(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "inc",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "clickInc"}},
                    ]},
                ]),
                file: Some("src/store.ts"),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "helper",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "run"}},
                    ]},
                ]),
                file: Some("src/util.ts"),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "helper",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                file: Some("src/decoy.ts"),
                short_names: true,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// react-hoc-component.test.ts:157 — emits jsx-render edges so getCallers/getImpactRadius resolve a forwardRef component
#[tokio::test]
async fn react_hoc_component_157() {
    check_case(Case {
        name: "react_hoc_component_157",
        files: vec![
            SourceFile {
                path: "button.tsx",
                source: "import * as React from 'react';\nexport const Button = React.forwardRef<HTMLButtonElement, {}>((props, ref) => <button ref={ref} {...props} />);\n",
            },
            SourceFile {
                path: "page.tsx",
                source: "import { Button } from './button';\nexport function Page() {\n  return <Button>Click</Button>;\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "Button",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Page"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// go-embedding-no-override.test.ts:224 — ends a walk through the embedded type's method at that method
#[tokio::test]
async fn go_embedding_no_override_224() {
    check_case(Case {
        name: "go_embedding_no_override_224",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: "module example.com/app\n\ngo 1.22\n",
            },
            SourceFile {
                path: "gin/gin.go",
                source: "package gin\n\ntype HandlerFunc func()\n\ntype HandlersChain []HandlerFunc\n\nfunc (c HandlersChain) Last() HandlerFunc { return nil }\n\ntype IRoutes interface {\n\tUse(...HandlerFunc) IRoutes\n}\n\ntype RouterGroup struct{ Handlers HandlersChain }\n\nfunc (group *RouterGroup) Use(middleware ...HandlerFunc) IRoutes {\n\tgroup.Handlers = append(group.Handlers, middleware...)\n\treturn group\n}\n\ntype Engine struct {\n\tRouterGroup\n\ttrees []string\n}\n\nfunc (engine *Engine) Use(middleware ...HandlerFunc) IRoutes {\n\tengine.RouterGroup.Use(middleware...)\n\tengine.rebuild404Handlers()\n\treturn engine\n}\n\nfunc (engine *Engine) rebuild404Handlers() {}\n\n// A Last of its own over the defined type it embeds.\ntype routeInfo struct {\n\tHandlersChain\n\tpath string\n}\n\nfunc (r routeInfo) Last() HandlerFunc { return nil }\n",
            },
            SourceFile {
                path: "concurrency/mutex.go",
                source: "package concurrency\n\ntype Locker interface {\n\tLock()\n\tUnlock()\n}\n\ntype Mutex struct{ key string }\n\nfunc (m *Mutex) Lock()   {}\nfunc (m *Mutex) Unlock() {}\n\ntype lockerMutex struct{ *Mutex }\n\nfunc (lm *lockerMutex) Lock()   { lm.Mutex.Lock() }\nfunc (lm *lockerMutex) Unlock() { lm.Mutex.Unlock() }\n",
            },
            SourceFile {
                path: "api/auth.go",
                source: "package api\n\ntype AuthServer interface {\n\tAuthenticate(name string) error\n\tUserAdd(name string) error\n}\n\ntype UnimplementedAuthServer struct{}\n\nfunc (UnimplementedAuthServer) Authenticate(name string) error { return nil }\nfunc (UnimplementedAuthServer) UserAdd(name string) error      { return nil }\n",
            },
            SourceFile {
                path: "client/mock.go",
                source: "package client\n\nimport \"example.com/app/api\"\n\ntype mockAuthServer struct {\n\tapi.UnimplementedAuthServer\n}\n\nfunc (mockAuthServer) Authenticate(name string) error { return nil }\n",
            },
            SourceFile {
                path: "storage/appender.go",
                source: "package storage\n\ntype Appender interface {\n\tAppend(v float64) error\n\tCommit() error\n}\n\ntype limitAppender struct {\n\tAppender\n\tlimit int\n}\n\nfunc (a *limitAppender) Append(v float64) error { return a.Appender.Append(v) }\n",
            },
            SourceFile {
                path: "java/src/main/java/app/Base.java",
                source: "package app;\n\npublic class Base {\n    public void run() {}\n}\n",
            },
            SourceFile {
                path: "java/src/main/java/app/Derived.java",
                source: "package app;\n\npublic class Derived extends Base {\n    @Override\n    public void run() {}\n}\n",
            },
            SourceFile {
                path: "web/shapes.ts",
                source: "export class Shape {\n  area(): number { return 0; }\n}\n\nexport class Square extends Shape {\n  area(): number { return 1; }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "RouterGroup::Use",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Engine::Use"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Engine::Use",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "RouterGroup::Use"}},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-member-receiver.test.ts:299 — the declaration in the caller’s own translation unit, of classes that share a name
#[tokio::test]
async fn cpp_member_receiver_299() {
    check_case(Case {
        name: "cpp_member_receiver_299",
        files: vec![
            SourceFile {
                path: "include/leveldb/slice.h",
                source: "namespace leveldb {\nclass Slice {\n public:\n  int size() const { return 0; }\n};\nclass Block {\n public:\n  int size() const { return 1; }\n};\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "helpers/memenv/memenv.cc",
                source: "#include \"include/leveldb/slice.h\"\nnamespace leveldb {\nclass FileState {\n public:\n  int Size() const;\n private:\n  Slice* blocks_;\n};\nint FileState::Size() const { return blocks_->size(); }\n}  // namespace leveldb\n",
            },
            SourceFile {
                path: "db/fault_injection_test.cc",
                source: "#include \"include/leveldb/slice.h\"\nnamespace leveldb {\nclass FileState {\n public:\n  int Size() const;\n private:\n  Block* blocks_;\n};\nint FileState::Size() const { return blocks_->size(); }\n}  // namespace leveldb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "leveldb::FileState::Size",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "leveldb::Block::size"}},
                    ]},
                ]),
                file: Some("db/fault_injection_test.cc"),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "leveldb::FileState::Size",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "leveldb::Slice::size"}},
                    ]},
                ]),
                file: Some("helpers/memenv/memenv.cc"),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-nested-template-receiver.test.ts:198 — the end of a declaration begun on the line above is read with that line, not as its template argument's type
#[tokio::test]
async fn cpp_nested_template_receiver_198() {
    // Checks call targets; upstream confidence/provenance fields are not exposed by zg.
    check_case(Case {
        name: "cpp_nested_template_receiver_198",
        files: vec![
            SourceFile {
                path: "util/autovector.h",
                source: "namespace rocksdb {\ntemplate <class T, size_t kSize = 8>\nclass autovector {\n public:\n  size_t size() const { return 0; }\n  void clear() {}\n  void push_back(const T& item) {}\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/compaction/compaction.h",
                source: "namespace rocksdb {\nstruct FileMetaData {};\nstruct CompactionInputFiles {\n  size_t size() const { return 0; }\n  void clear() {}\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/version_set.h",
                source: "#include \"util/autovector.h\"\n#include \"db/compaction/compaction.h\"\nnamespace rocksdb {\nclass VersionEdit {};\nclass VersionStorageInfo {\n public:\n  void ComputeFilesMarkedForCompaction();\n  size_t CountEditLists(VersionEdit* edit);\n private:\n  autovector<std::pair<int, FileMetaData*>> files_marked_for_compaction_;\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/version_set.cc",
                source: "#include \"db/version_set.h\"\nnamespace rocksdb {\nvoid VersionStorageInfo::ComputeFilesMarkedForCompaction() {\n  files_marked_for_compaction_.clear();\n}\nsize_t VersionStorageInfo::CountEditLists(VersionEdit* edit) {\n  autovector<autovector<VersionEdit*>> edit_lists;\n  edit_lists.push_back(autovector<VersionEdit*>());\n  return edit_lists.size();\n}\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "table/block_based/cachable_entry.h",
                source: "namespace rocksdb {\ntemplate <class T>\nclass CachableEntry {\n public:\n  size_t size() const { return 0; }\n};\n}  // namespace rocksdb\n",
            },
            SourceFile {
                path: "db/blob/blob_file_reader.cc",
                source: "#include \"util/autovector.h\"\n#include \"table/block_based/cachable_entry.h\"\nnamespace rocksdb {\nstruct BlobContents {};\nstruct BlobReadRequest {};\nsize_t MultiGetBlob(\n    autovector<std::pair<BlobReadRequest*,\n                         CachableEntry<BlobContents>>>& blob_reqs) {\n  return blob_reqs.size();\n}\n}  // namespace rocksdb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rocksdb::MultiGetBlob",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "rocksdb::autovector::size"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-defined-methods.test.ts:256 — an explicit specialization a macro defines is named without its template arguments
#[tokio::test]
async fn cpp_macro_defined_methods_256() {
    check_case(Case {
        name: "cpp_macro_defined_methods_256",
        files: vec![
            SourceFile {
                path: "src/wire.h",
                source: "class Wire {\n public:\n  template <typename T, int Kind> static bool Read(T* value);\n};\n#define READ_PRIMITIVE(CPPTYPE, KIND) \\\n  template <> inline bool Wire::Read<CPPTYPE, KIND>(CPPTYPE* value) { return true; }\nREAD_PRIMITIVE(int, 1)\nREAD_PRIMITIVE(double, 2)\n",
            },
            SourceFile {
                path: "src/use.cc",
                source: "#include \"src/wire.h\"\nbool Use(int* value) { return Wire::Read<int, 1>(value); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Use",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Wire::Read", "filePath": "src/wire.h", "startLine": [7, 8]}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:350 — chooses among overloads by arity only when exactly one admits the argument count
#[tokio::test]
async fn cpp_macro_and_constructor_calls_350() {
    // Upstream identifies constructor overloads by signature; these assertions identify the same declarations by source line.
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_350",
        files: vec![
            SourceFile {
                path: "overloads.cpp",
                source: "struct Widget {\n  Widget() {}\n  Widget(int value) {}\n  Widget(int a, int b = 2) {}\n};\nstruct Ambiguous {\n  Ambiguous(int) {}\n  Ambiguous(double) {}\n};\nvoid default_use() { Widget w; }\nvoid two_use() { Widget w(1, 2); }\nvoid one_use() { Widget w(1); }\nvoid ambiguous_use(int value) { Ambiguous w(value); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "default_use",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Widget::Widget", "filePath": "overloads.cpp", "startLine": 2}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "two_use",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Widget::Widget", "filePath": "overloads.cpp", "startLine": 4}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "one_use",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ambiguous_use",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:432 — constructs default and explicitly braced array elements, retaining nested argument calls
#[tokio::test]
async fn cpp_macro_and_constructor_calls_432() {
    // Upstream identifies overloads by signature; the source lines select those same declarations.
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_432",
        files: vec![
            SourceFile {
                path: "arrays.cpp",
                source: "struct Widget {\n Widget() {}\n Widget(int value) {}\n};\nint argument() { return 1; }\nvoid plain() { Widget items[2]; }\nvoid empty() { Widget items[2]{}; }\nvoid elements() { Widget items[3]{{argument()}, {2}}; }\nvoid grid() { Widget items[2][2]{{{1}, {2}}, {{3}}}; }\nvoid scalar_elements() { Widget items[2]{1, 2}; }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "plain",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Widget::Widget", "startLine": 2}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "empty",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Widget::Widget", "startLine": 2}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "grid",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Widget::Widget", "startLine": 2}},
                        {"symbol": {"name": "Widget::Widget", "startLine": 3}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "scalar_elements",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Widget::Widget", "startLine": 3}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "elements",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "Widget::Widget", "startLine": 2}},
                        {"symbol": {"name": "Widget::Widget", "startLine": 3}},
                        {"symbol": {"name": "argument"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-type-alias-receiver.test.ts:468 — a class template's specialization can declare a member the template itself does not
#[tokio::test]
async fn cpp_type_alias_receiver_468() {
    check_case(Case {
        name: "cpp_type_alias_receiver_468",
        files: vec![
            SourceFile {
                path: "util/omt.h",
                source: "namespace toku {\nnamespace omt_internal {\ntemplate <typename omtdata_t, bool subtree_supports_marks>\nclass omt_node_templated {\n public:\n  void clear_stolen_bits() {}\n};\ntemplate <typename omtdata_t>\nclass omt_node_templated<omtdata_t, true> {\n public:\n  bool get_marked() const { return false; }\n};\n}  // namespace omt_internal\ntemplate <typename omtdata_t, bool supports_marks = false>\nclass omt {\n public:\n  bool has_marks() const;\n private:\n  typedef omt_internal::omt_node_templated<omtdata_t, supports_marks> omt_node;\n  omt_node root_;\n};\ntemplate <typename omtdata_t, bool supports_marks>\nbool omt<omtdata_t, supports_marks>::has_marks() const {\n  const omt_node &node = root_;\n  return node.get_marked();\n}\n}  // namespace toku\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "toku::omt::has_marks",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "get_marked"}},
                    ]},
                ]),
                short_names: true,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-specialization-names.test.ts:384 — the specialization the call writes the arguments of
// cpp-specialization-names.test.ts:408 — the specializations a macro defines
// cpp-specialization-names.test.ts:413 — a specialization when nothing else is there
// cpp-specialization-names.test.ts:418 — the template itself for arguments no specialization writes
// cpp-specialization-names.test.ts:433 — a constructor is no specialization
// cpp-specialization-names.test.ts:437 — the template itself for a call that writes no arguments
// cpp-specialization-names.test.ts:442 — a call inside the class, its arguments written unqualified
// cpp-specialization-names.test.ts:446 — nothing when only specializations are there and none is the one called
// cpp-specialization-names.test.ts:455 — a derived class's namesake overrides the template, not its specialization
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn cpp_specialization_names_384() {
    check_case(Case {
        name: "cpp_specialization_names_384",
        files: vec![
            SourceFile {
                path: "src/wire.h",
                source: "namespace pb {\nclass WireFormatLite {\n public:\n  enum FieldType { TYPE_INT32 = 1, TYPE_INT64 = 2 };\n  template <typename CType, enum FieldType DeclaredType>\n  static bool ReadPrimitive(int* input, CType* value);\n  template <typename T> T get() const;\n  bool operator<(const WireFormatLite& other) const;\n};\n\ntemplate <>\ninline bool WireFormatLite::ReadPrimitive<int, WireFormatLite::TYPE_INT32>(int* input, int* value) {\n  return true;\n}\ntemplate <>\ninline bool WireFormatLite::ReadPrimitive<long, WireFormatLite::TYPE_INT64>(int* input, long* value) {\n  return false;\n}\ntemplate <>\ninline int WireFormatLite::get<int>() const { return 1; }\ninline bool WireFormatLite::operator<(const WireFormatLite& other) const { return false; }\n\ntemplate <typename T> void Swap(T& a, T& b) {}\ntemplate <> void Swap<WireFormatLite>(WireFormatLite& a, WireFormatLite& b) {}\ntemplate <typename T> T* Make() { return nullptr; }\ntemplate <> WireFormatLite* Make<WireFormatLite>() { return nullptr; }\n\ntemplate <typename T> struct Box { T get() const; };\ntemplate <> int Box<int>::get() const { return 0; }\ntemplate <typename T> T Box<T>::get() const { return T(); }\n}  // namespace pb\n",
            },
            SourceFile {
                path: "src/use.cc",
                source: "#include \"src/wire.h\"\nnamespace pb {\nbool Read(int* in, int* v) { return WireFormatLite::ReadPrimitive<int, WireFormatLite::TYPE_INT32>(in, v); }\nbool ReadLong(int* in, long* v) { return WireFormatLite::ReadPrimitive<long, WireFormatLite::TYPE_INT64>(in, v); }\nbool ReadLongQualified(int* in, long* v) { return WireFormatLite::ReadPrimitive<long, pb::WireFormatLite::TYPE_INT64>(in, v); }\nint Get(const WireFormatLite& w) { return w.get<int>(); }\nvoid SwapWire(WireFormatLite& a, WireFormatLite& b) { Swap<WireFormatLite>(a, b); }\nvoid SwapInts(int& x, int& y) { Swap<int>(x, y); }\nvoid SwapDeduced(int& x, int& y) { Swap(x, y); }\nint* MakeInt() { return Make<int>(); }\nWireFormatLite* MakeWire() { return Make<WireFormatLite>(); }\n}  // namespace pb\n",
            },
            SourceFile {
                path: "src/reader.h",
                source: "#include \"src/wire.h\"\nnamespace pb {\nclass Reader : public WireFormatLite {\n public:\n  bool ReadLong(int* in) { return ReadPrimitive<long, TYPE_INT64>(in, nullptr); }\n  template <typename C, FieldType D> bool ReadAny(int* in, C* v) { return ReadPrimitive<C, D>(in, v); }\n};\n}  // namespace pb\n",
            },
            SourceFile {
                path: "src/serialize.h",
                source: "namespace ns {\ntemplate <typename T> void Serialize(T val, char* dest);\ntemplate <> inline void Serialize<char>(char t, char* dest) {}\ntemplate <> inline void Serialize<long>(long t, char* dest) {}\ntemplate <> inline void Serialize<const char*>(const char* t, char* dest) {}\ninline void WriteLocal(char* dest) { ns::Serialize<long>(1, dest); }\n}  // namespace ns\n",
            },
            SourceFile {
                path: "src/format.cc",
                source: "#include \"src/serialize.h\"\nnamespace ns {\nvoid Write(char* dest) { ns::Serialize<long>(1, dest); }\nvoid WriteShort(char* dest) { ns::Serialize<short>(1, dest); }\nvoid WriteDeduced(char* dest) { ns::Serialize('c', dest); }\nvoid WriteBrackets(char* dest) { ns::Serialize<>('c', dest); }\nvoid WriteText(char* dest) { ns::Serialize<const char *>(\"x\", dest); }\nvoid WriteGlobal(char* dest) { ::ns::Serialize<long>(1, dest); }\n}  // namespace ns\n",
            },
            SourceFile {
                path: "src/parse.h",
                source: "namespace fmt {\ntemplate <typename T> T parse(const char* text);\ntemplate <> int parse<int>(const char* text) { return 0; }\ninline long parse_long(const char* t) { return parse<long>(t); }\n}  // namespace fmt\n",
            },
            SourceFile {
                path: "src/digits.h",
                source: "#define MY_BEGIN_NAMESPACE namespace mylib {\n#define MY_END_NAMESPACE }\nMY_BEGIN_NAMESPACE\ntemplate <typename T> constexpr int digits() { return 1; }\ntemplate <> constexpr int digits<long>() { return 19; }\nMY_END_NAMESPACE\n",
            },
            SourceFile {
                path: "src/digits.cc",
                source: "#include \"src/digits.h\"\nint long_digits() { return mylib::digits<long>(); }\nint char_digits() { return mylib::digits<char>(); }\n",
            },
            SourceFile {
                path: "src/action.h",
                source: "namespace gm {\ntemplate <typename F> class ActionInterface {};\ntemplate <typename F> class Action {\n public:\n  explicit Action(ActionInterface<F>* impl) {}\n  template <typename Func> explicit Action(const Action<Func>& action) {}\n};\ntemplate <typename F> Action<F> Convert(const Action<long>& from) { return Action<F>(from); }\n}  // namespace gm\n",
            },
            SourceFile {
                path: "src/get.h",
                source: "namespace kv {\nstruct GetResult {};\ntemplate <typename T> GetResult Get(int key) { return GetResult(); }\ntemplate <> GetResult Get<long>(int key) { return GetResult(); }\ninline GetResult GetInt() { return Get<int>(1); }\n}  // namespace kv\n",
            },
            SourceFile {
                path: "src/arena.h",
                source: "namespace pb {\nclass Arena;\ntypedef Arena* ArenaPtr;\nclass Arena {\n public:\n  template <typename T> static void* DefaultConstruct(ArenaPtr arena);\n  template <typename T> static T* Create(Arena* arena) { return static_cast<T*>(DefaultConstruct<T>(arena)); }\n};\ntemplate <typename T> void* Arena::DefaultConstruct(ArenaPtr arena) { return nullptr; }\n}  // namespace pb\n",
            },
            SourceFile {
                path: "src/registry.h",
                source: "namespace pb {\nclass Handle;\ntypedef Handle* HandlePtr;\nclass Registry {\n public:\n  template <typename T> void Add(Handle* h) {}\n};\ntemplate <> inline void Registry::Add<int>(HandlePtr h) {}\nclass Pool : public Registry {\n public:\n  void AddInt(Handle* h) { Add<int>(h); }\n};\ninline void Register(Registry& r, Handle* h) { r.Add<int>(h); }\n}  // namespace pb\n",
            },
            SourceFile {
                path: "src/bits.h",
                source: "namespace fmt {\ntemplate <typename T> constexpr int num_bits() { return 8; }\ntemplate <> constexpr int num_bits<long>() { return 64; }\ntemplate <> constexpr int num_bits<short>() { return 16; }\ninline int char_bits() { return num_bits<char>(); }\ninline int long_bits() { return num_bits<long>(); }\ntemplate <typename T> int width(T value) { return 1; }\ntemplate <> int width<long>(long value) { return 8; }\ninline int int_width(int v) { return width(v); }\ninline int named_long_bits() { const char* unit = \"größe\"; return num_bits<long>(); }\ntemplate <typename T, bool zigzag = false> T decode(T v) { return v; }\ntemplate <> int decode<int, true>(int v) { return -v; }\ninline int plain(int v) { return decode<int>(v); }\ntemplate <typename T> int scale(T v);\ntemplate <> int scale<long>(long v) { return 2; } template <typename T> int scale(T v) { return 1; }\ninline int int_scale() { return scale<int>(1); }\ntemplate <> long decode<long>(long v) { return 2 * v; }\ninline long zigzag(long v) { return decode<long, true>(v); }\n}  // namespace fmt\n",
            },
            SourceFile {
                path: "src/codec.h",
                source: "#include \"src/wire.h\"\nnamespace pb {\nclass Codec {\n public:\n  template <typename T> void Put(T v) {}\n};\nclass IntCodec : public Codec {\n public:\n  template <typename T> void Put(T v);\n  WireFormatLite& target() { return w_; }\n  WireFormatLite w_;\n};\ntemplate <> inline void IntCodec::Put<int>(int v) {}\ninline void PutInt(IntCodec& c) { c.Put<int>(1); }\ninline void PutLong(IntCodec& c) { c.Put<long>(1L); }\ninline long LongFromCodec(IntCodec& codec) { return codec.target().get<long>(); }\n}  // namespace pb\n",
            },
            SourceFile {
                path: "src/reflection.h",
                source: "namespace pb {\nclass Reflection {\n public:\n  template <typename T> const T& GetField(int index) const;\n};\ntemplate <typename T> const T& Reflection::GetField(int index) const { static T t; return t; }\n#define DEFINE_FIELD_GETTER(TYPE) \\\n  template <> const TYPE& Reflection::GetField<TYPE>(int index) const { static TYPE t; return t; }\nDEFINE_FIELD_GETTER(int)\nDEFINE_FIELD_GETTER(long)\ninline long GetLong(const Reflection& r) { return r.GetField<long>(0); }\ninline double GetDouble(const Reflection& r) { return r.GetField<double>(0); }\n}  // namespace pb\n",
            },
            SourceFile {
                path: "src/arena_test.cc",
                source: "#include \"src/arena.h\"\nnamespace pb {\nstruct Hooked {};\ntemplate <> void* Arena::DefaultConstruct<Hooked>(Arena* arena) { return nullptr; }\nvoid* MakeHooked(Arena* a) { return Arena::DefaultConstruct<Hooked>(a); }\n}  // namespace pb\n",
            },
            SourceFile {
                path: "src/repeated.h",
                source: "namespace pb {\nclass FieldBase {\n public:\n  template <typename T> void MergeFrom(const FieldBase& from) {}\n  virtual void Visit(int value) {}\n};\ntemplate <> inline void FieldBase::MergeFrom<int>(const FieldBase& from) {}\nclass Field : public FieldBase {\n public:\n  void MergeFrom(const Field& other) {}\n  template <typename T> void Visit(T value) {}\n};\ntemplate <> inline void Field::Visit<int>(int value) {}\n}  // namespace pb\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "pb::Read",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::WireFormatLite::ReadPrimitive", "startLine": 12}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::ReadLong",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::WireFormatLite::ReadPrimitive", "startLine": 16}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::Get",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::WireFormatLite::get", "startLine": 20}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::SwapWire",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::Swap", "startLine": 24}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::MakeWire",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::Make", "startLine": 26}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ns::Write",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "ns::Serialize", "startLine": 4}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::MakeHooked",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::Arena::DefaultConstruct", "startLine": 4}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fmt::long_bits",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "fmt::num_bits", "startLine": 3}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::Register",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::Registry::Add", "startLine": 8}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::Pool::AddInt",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::Registry::Add", "startLine": 8}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::PutInt",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::IntCodec::Put", "startLine": 13}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fmt::named_long_bits",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "fmt::num_bits", "startLine": 3}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::ReadLongQualified",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::WireFormatLite::ReadPrimitive", "startLine": 16}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ns::WriteText",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "ns::Serialize", "startLine": 5}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ns::WriteGlobal",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "ns::Serialize", "startLine": 4}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ns::WriteLocal",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "ns::Serialize", "startLine": 4}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "long_digits",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "digits", "startLine": 5}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::GetLong",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::Reflection::GetField", "startLine": 10}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::GetDouble",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::Reflection::GetField", "startLine": 6}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ns::WriteDeduced",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "ns::Serialize", "startLine": 3}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ns::WriteBrackets",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "ns::Serialize", "startLine": 3}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::SwapInts",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::Swap", "startLine": 23}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::MakeInt",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::Make", "startLine": 25}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fmt::char_bits",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "fmt::num_bits", "startLine": 2}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::Arena::Create",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::Arena::DefaultConstruct", "startLine": 9}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fmt::plain",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "fmt::decode", "startLine": 11}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fmt::zigzag",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "fmt::decode", "startLine": 11}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fmt::int_scale",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "fmt::scale", "startLine": 15}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "char_digits",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "digits", "startLine": 4}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "kv::GetInt",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "kv::Get", "startLine": 3}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "gm::Convert",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "gm::Action::Action", "startLine": 6}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::SwapDeduced",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::Swap", "startLine": 23}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fmt::int_width",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "fmt::width", "startLine": 7}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::Reader::ReadLong",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::WireFormatLite::ReadPrimitive", "startLine": 16}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::Reader::ReadAny",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ns::WriteShort",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::PutLong",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "pb::LongFromCodec",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "pb::IntCodec::target", "startLine": 10}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fmt::parse_long",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-include-walk.test.ts:104 — the macro reached through value-guarded headers still does not bind to the decoy function
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn cpp_macro_include_walk_104() {
    check_case(Case {
        name: "cpp_macro_include_walk_104",
        files: vec![
            SourceFile {
                path: "h0_a.h",
                source: "#ifndef H_0_a\n#define H_0_a 1\n#include \"h1_a.h\"\n#include \"h1_b.h\"\n#define M_0_a(x) (x)\nint f_0_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h0_b.h",
                source: "#ifndef H_0_b\n#define H_0_b 1\n#include \"h1_a.h\"\n#include \"h1_b.h\"\n#define M_0_b(x) (x)\nint f_0_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h1_a.h",
                source: "#ifndef H_1_a\n#define H_1_a 1\n#include \"h2_a.h\"\n#include \"h2_b.h\"\n#define M_1_a(x) (x)\nint f_1_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h1_b.h",
                source: "#ifndef H_1_b\n#define H_1_b 1\n#include \"h2_a.h\"\n#include \"h2_b.h\"\n#define M_1_b(x) (x)\nint f_1_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h2_a.h",
                source: "#ifndef H_2_a\n#define H_2_a 1\n#include \"h3_a.h\"\n#include \"h3_b.h\"\n#define M_2_a(x) (x)\nint f_2_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h2_b.h",
                source: "#ifndef H_2_b\n#define H_2_b 1\n#include \"h3_a.h\"\n#include \"h3_b.h\"\n#define M_2_b(x) (x)\nint f_2_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h3_a.h",
                source: "#ifndef H_3_a\n#define H_3_a 1\n#include \"h4_a.h\"\n#include \"h4_b.h\"\n#define M_3_a(x) (x)\nint f_3_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h3_b.h",
                source: "#ifndef H_3_b\n#define H_3_b 1\n#include \"h4_a.h\"\n#include \"h4_b.h\"\n#define M_3_b(x) (x)\nint f_3_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h4_a.h",
                source: "#ifndef H_4_a\n#define H_4_a 1\n#include \"h5_a.h\"\n#include \"h5_b.h\"\n#define M_4_a(x) (x)\nint f_4_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h4_b.h",
                source: "#ifndef H_4_b\n#define H_4_b 1\n#include \"h5_a.h\"\n#include \"h5_b.h\"\n#define M_4_b(x) (x)\nint f_4_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h5_a.h",
                source: "#ifndef H_5_a\n#define H_5_a 1\n#include \"h6_a.h\"\n#include \"h6_b.h\"\n#define M_5_a(x) (x)\nint f_5_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h5_b.h",
                source: "#ifndef H_5_b\n#define H_5_b 1\n#include \"h6_a.h\"\n#include \"h6_b.h\"\n#define M_5_b(x) (x)\nint f_5_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h6_a.h",
                source: "#ifndef H_6_a\n#define H_6_a 1\n#include \"h7_a.h\"\n#include \"h7_b.h\"\n#define M_6_a(x) (x)\nint f_6_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h6_b.h",
                source: "#ifndef H_6_b\n#define H_6_b 1\n#include \"h7_a.h\"\n#include \"h7_b.h\"\n#define M_6_b(x) (x)\nint f_6_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h7_a.h",
                source: "#ifndef H_7_a\n#define H_7_a 1\n#include \"h8_a.h\"\n#include \"h8_b.h\"\n#define M_7_a(x) (x)\nint f_7_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h7_b.h",
                source: "#ifndef H_7_b\n#define H_7_b 1\n#include \"h8_a.h\"\n#include \"h8_b.h\"\n#define M_7_b(x) (x)\nint f_7_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h8_a.h",
                source: "#ifndef H_8_a\n#define H_8_a 1\n#include \"h9_a.h\"\n#include \"h9_b.h\"\n#define M_8_a(x) (x)\nint f_8_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h8_b.h",
                source: "#ifndef H_8_b\n#define H_8_b 1\n#include \"h9_a.h\"\n#include \"h9_b.h\"\n#define M_8_b(x) (x)\nint f_8_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h9_a.h",
                source: "#ifndef H_9_a\n#define H_9_a 1\n#include \"h10_a.h\"\n#include \"h10_b.h\"\n#define M_9_a(x) (x)\nint f_9_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h9_b.h",
                source: "#ifndef H_9_b\n#define H_9_b 1\n#include \"h10_a.h\"\n#include \"h10_b.h\"\n#define M_9_b(x) (x)\nint f_9_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h10_a.h",
                source: "#ifndef H_10_a\n#define H_10_a 1\n#include \"h11_a.h\"\n#include \"h11_b.h\"\n#define M_10_a(x) (x)\nint f_10_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h10_b.h",
                source: "#ifndef H_10_b\n#define H_10_b 1\n#include \"h11_a.h\"\n#include \"h11_b.h\"\n#define M_10_b(x) (x)\nint f_10_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h11_a.h",
                source: "#ifndef H_11_a\n#define H_11_a 1\n#include \"h12_a.h\"\n#include \"h12_b.h\"\n#define M_11_a(x) (x)\nint f_11_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h11_b.h",
                source: "#ifndef H_11_b\n#define H_11_b 1\n#include \"h12_a.h\"\n#include \"h12_b.h\"\n#define M_11_b(x) (x)\nint f_11_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h12_a.h",
                source: "#ifndef H_12_a\n#define H_12_a 1\n#include \"h13_a.h\"\n#include \"h13_b.h\"\n#define M_12_a(x) (x)\nint f_12_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h12_b.h",
                source: "#ifndef H_12_b\n#define H_12_b 1\n#include \"h13_a.h\"\n#include \"h13_b.h\"\n#define M_12_b(x) (x)\nint f_12_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h13_a.h",
                source: "#ifndef H_13_a\n#define H_13_a 1\n#include \"h14_a.h\"\n#include \"h14_b.h\"\n#define M_13_a(x) (x)\nint f_13_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h13_b.h",
                source: "#ifndef H_13_b\n#define H_13_b 1\n#include \"h14_a.h\"\n#include \"h14_b.h\"\n#define M_13_b(x) (x)\nint f_13_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h14_a.h",
                source: "#ifndef H_14_a\n#define H_14_a 1\n#include \"h15_a.h\"\n#include \"h15_b.h\"\n#define M_14_a(x) (x)\nint f_14_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h14_b.h",
                source: "#ifndef H_14_b\n#define H_14_b 1\n#include \"h15_a.h\"\n#include \"h15_b.h\"\n#define M_14_b(x) (x)\nint f_14_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h15_a.h",
                source: "#ifndef H_15_a\n#define H_15_a 1\n#include \"h16_a.h\"\n#include \"h16_b.h\"\n#define M_15_a(x) (x)\nint f_15_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h15_b.h",
                source: "#ifndef H_15_b\n#define H_15_b 1\n#include \"h16_a.h\"\n#include \"h16_b.h\"\n#define M_15_b(x) (x)\nint f_15_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h16_a.h",
                source: "#ifndef H_16_a\n#define H_16_a 1\n#include \"h17_a.h\"\n#include \"h17_b.h\"\n#define M_16_a(x) (x)\nint f_16_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h16_b.h",
                source: "#ifndef H_16_b\n#define H_16_b 1\n#include \"h17_a.h\"\n#include \"h17_b.h\"\n#define M_16_b(x) (x)\nint f_16_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h17_a.h",
                source: "#ifndef H_17_a\n#define H_17_a 1\n#include \"h18_a.h\"\n#include \"h18_b.h\"\n#define M_17_a(x) (x)\nint f_17_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h17_b.h",
                source: "#ifndef H_17_b\n#define H_17_b 1\n#include \"h18_a.h\"\n#include \"h18_b.h\"\n#define M_17_b(x) (x)\nint f_17_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h18_a.h",
                source: "#ifndef H_18_a\n#define H_18_a 1\n#include \"h19_a.h\"\n#include \"h19_b.h\"\n#define M_18_a(x) (x)\nint f_18_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h18_b.h",
                source: "#ifndef H_18_b\n#define H_18_b 1\n#include \"h19_a.h\"\n#include \"h19_b.h\"\n#define M_18_b(x) (x)\nint f_18_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h19_a.h",
                source: "#ifndef H_19_a\n#define H_19_a 1\n#include \"h20_a.h\"\n#include \"h20_b.h\"\n#define M_19_a(x) (x)\nint f_19_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h19_b.h",
                source: "#ifndef H_19_b\n#define H_19_b 1\n#include \"h20_a.h\"\n#include \"h20_b.h\"\n#define M_19_b(x) (x)\nint f_19_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h20_a.h",
                source: "#ifndef H_20_a\n#define H_20_a 1\n#include \"h21_a.h\"\n#include \"h21_b.h\"\n#define M_20_a(x) (x)\nint f_20_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h20_b.h",
                source: "#ifndef H_20_b\n#define H_20_b 1\n#include \"h21_a.h\"\n#include \"h21_b.h\"\n#define M_20_b(x) (x)\nint f_20_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h21_a.h",
                source: "#ifndef H_21_a\n#define H_21_a 1\n#include \"h22_a.h\"\n#include \"h22_b.h\"\n#define M_21_a(x) (x)\nint f_21_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h21_b.h",
                source: "#ifndef H_21_b\n#define H_21_b 1\n#include \"h22_a.h\"\n#include \"h22_b.h\"\n#define M_21_b(x) (x)\nint f_21_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h22_a.h",
                source: "#ifndef H_22_a\n#define H_22_a 1\n#include \"h23_a.h\"\n#include \"h23_b.h\"\n#define M_22_a(x) (x)\nint f_22_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h22_b.h",
                source: "#ifndef H_22_b\n#define H_22_b 1\n#include \"h23_a.h\"\n#include \"h23_b.h\"\n#define M_22_b(x) (x)\nint f_22_b(int);\n#endif\n",
            },
            SourceFile {
                path: "h23_a.h",
                source: "#ifndef H_23_a\n#define H_23_a 1\n#define M_23_a(x) (x)\nint f_23_a(int);\n#endif\n",
            },
            SourceFile {
                path: "h23_b.h",
                source: "#ifndef H_23_b\n#define H_23_b 1\n#define M_23_b(x) (x)\nint f_23_b(int);\n#endif\n",
            },
            SourceFile {
                path: "trace.h",
                source: "#define TRACE(v) ((void)(v))\n",
            },
            SourceFile {
                path: "unit.cpp",
                source: "#include \"h0_a.h\"\n#include \"h0_b.h\"\n#include \"trace.h\"\nvoid unit() { TRACE(1); }\n",
            },
            SourceFile {
                path: "decoy.cpp",
                source: "void TRACE(int v) {}\nvoid caller() { TRACE(2); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "unit",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"edges": [
                        {"symbol": {"filePath": "decoy.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:73 — suppresses macros visible through nested includes, a sibling header, a local #define
// honors #undef (c)
#[tokio::test]
async fn cpp_macro_and_constructor_calls_73_c() {
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_73_c",
        files: vec![
            SourceFile {
                path: "src/pg/pinio.h",
                source: "#define HEADER_TRACE(v) ((void)(v))\n",
            },
            SourceFile {
                path: "src/drivers/pinio.h",
                source: "// unrelated header with the same basename\n",
            },
            SourceFile {
                path: "src/pg/pinio.c",
                source: "#include \"pinio.h\"\nvoid sibling_header_use() { HEADER_TRACE(1); }\n",
            },
            SourceFile {
                path: "inner.h",
                source: "#define TRACE_POINT(value) ((void)(value))\n",
            },
            SourceFile {
                path: "outer.h",
                source: "#include \"inner.h\"\n",
            },
            SourceFile {
                path: "exercise.c",
                source: "#include \"outer.h\"\nvoid macro_use() { TRACE_POINT(1); }\nvoid local_macro() {\n#define INNER_TRACE(v) ((void)(v))\nINNER_TRACE(1);\n}\n#undef TRACE_POINT\nvoid after_undef() { TRACE_POINT(1); }\n",
            },
            SourceFile {
                path: "decoy.c",
                source: "void HEADER_TRACE(int value) {}\nvoid INNER_TRACE(int value) {}\nvoid TRACE_POINT(int value) {}\nvoid unrelated_use() { TRACE_POINT(1); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "macro_use",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "sibling_header_use",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "local_macro",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "after_undef",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "TRACE_POINT", "filePath": "decoy.c"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "unrelated_use",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "TRACE_POINT", "filePath": "decoy.c"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:73 — suppresses macros visible through nested includes, a sibling header, a local #define
// honors #undef (cpp)
#[tokio::test]
async fn cpp_macro_and_constructor_calls_73_cpp() {
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_73_cpp",
        files: vec![
            SourceFile {
                path: "src/pg/pinio.h",
                source: "#define HEADER_TRACE(v) ((void)(v))\n",
            },
            SourceFile {
                path: "src/drivers/pinio.h",
                source: "// unrelated header with the same basename\n",
            },
            SourceFile {
                path: "src/pg/pinio.cpp",
                source: "#include \"pinio.h\"\nvoid sibling_header_use() { HEADER_TRACE(1); }\n",
            },
            SourceFile {
                path: "inner.h",
                source: "#define TRACE_POINT(value) ((void)(value))\n",
            },
            SourceFile {
                path: "outer.h",
                source: "#include \"inner.h\"\n",
            },
            SourceFile {
                path: "exercise.cpp",
                source: "#include \"outer.h\"\nvoid macro_use() { TRACE_POINT(1); }\nvoid local_macro() {\n#define INNER_TRACE(v) ((void)(v))\nINNER_TRACE(1);\n}\n#undef TRACE_POINT\nvoid after_undef() { TRACE_POINT(1); }\n",
            },
            SourceFile {
                path: "decoy.cpp",
                source: "void HEADER_TRACE(int value) {}\nvoid INNER_TRACE(int value) {}\nvoid TRACE_POINT(int value) {}\nvoid unrelated_use() { TRACE_POINT(1); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "macro_use",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "sibling_header_use",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "local_macro",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "after_undef",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "TRACE_POINT", "filePath": "decoy.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "unrelated_use",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "TRACE_POINT", "filePath": "decoy.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:114 — replays unguarded includes and respects guards, pragma once and changed flags (c)
#[tokio::test]
async fn cpp_macro_and_constructor_calls_114_c() {
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_114_c",
        files: vec![
            SourceFile {
                path: "unguarded.h",
                source: "#define TRACE(v) ((void)(v))\n",
            },
            SourceFile {
                path: "guarded.h",
                source: "#ifndef GUARDED_H\n#define GUARDED_H\n#define GUARDED(v) ((void)(v))\n#endif\n",
            },
            SourceFile {
                path: "once.h",
                source: "#pragma once\n#define ONCE(v) ((void)(v))\n",
            },
            SourceFile {
                path: "conditional.h",
                source: "#if ENABLE_TRACE\n#define CONDITIONAL(v) ((void)(v))\n#endif\n",
            },
            SourceFile {
                path: "unit.c",
                source: "#include \"unguarded.h\"\n#undef TRACE\nvoid between() { TRACE(1); }\n#include \"unguarded.h\"\nvoid repeated() { TRACE(1); }\n#include \"guarded.h\"\n#undef GUARDED\n#include \"guarded.h\"\nvoid guarded() { GUARDED(1); }\n#undef GUARDED_H\n#include \"guarded.h\"\nvoid reset_guard() { GUARDED(1); }\n#include \"once.h\"\n#undef ONCE\n#include \"once.h\"\nvoid once() { ONCE(1); }\n#define ENABLE_TRACE 0\n#include \"conditional.h\"\n#undef ENABLE_TRACE\n#define ENABLE_TRACE 1\n#include \"conditional.h\"\nvoid changed_flag() { CONDITIONAL(1); }\n",
            },
            SourceFile {
                path: "decoy.c",
                source: "void TRACE(int x) {}\nvoid GUARDED(int x) {}\nvoid ONCE(int x) {}\nvoid CONDITIONAL(int x) {}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "between",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "TRACE", "filePath": "decoy.c"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "repeated",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "guarded",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "GUARDED", "filePath": "decoy.c"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "reset_guard",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "once",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "ONCE", "filePath": "decoy.c"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "changed_flag",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:114 — replays unguarded includes and respects guards, pragma once and changed flags (cpp)
#[tokio::test]
async fn cpp_macro_and_constructor_calls_114_cpp() {
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_114_cpp",
        files: vec![
            SourceFile {
                path: "unguarded.h",
                source: "#define TRACE(v) ((void)(v))\n",
            },
            SourceFile {
                path: "guarded.h",
                source: "#ifndef GUARDED_H\n#define GUARDED_H\n#define GUARDED(v) ((void)(v))\n#endif\n",
            },
            SourceFile {
                path: "once.h",
                source: "#pragma once\n#define ONCE(v) ((void)(v))\n",
            },
            SourceFile {
                path: "conditional.h",
                source: "#if ENABLE_TRACE\n#define CONDITIONAL(v) ((void)(v))\n#endif\n",
            },
            SourceFile {
                path: "unit.cpp",
                source: "#include \"unguarded.h\"\n#undef TRACE\nvoid between() { TRACE(1); }\n#include \"unguarded.h\"\nvoid repeated() { TRACE(1); }\n#include \"guarded.h\"\n#undef GUARDED\n#include \"guarded.h\"\nvoid guarded() { GUARDED(1); }\n#undef GUARDED_H\n#include \"guarded.h\"\nvoid reset_guard() { GUARDED(1); }\n#include \"once.h\"\n#undef ONCE\n#include \"once.h\"\nvoid once() { ONCE(1); }\n#define ENABLE_TRACE 0\n#include \"conditional.h\"\n#undef ENABLE_TRACE\n#define ENABLE_TRACE 1\n#include \"conditional.h\"\nvoid changed_flag() { CONDITIONAL(1); }\n",
            },
            SourceFile {
                path: "decoy.cpp",
                source: "void TRACE(int x) {}\nvoid GUARDED(int x) {}\nvoid ONCE(int x) {}\nvoid CONDITIONAL(int x) {}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "between",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "TRACE", "filePath": "decoy.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "repeated",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "guarded",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "GUARDED", "filePath": "decoy.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "reset_guard",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "once",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "ONCE", "filePath": "decoy.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "changed_flag",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:516 — reproduction: the #ifdef branch after it stays possible (c)
#[tokio::test]
async fn cpp_macro_and_constructor_calls_516_c() {
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_516_c",
        files: vec![
            SourceFile {
                path: "config.h",
                source: "#if FREE_THREADED == 0\n#undef FREE_THREADED\n#endif\n",
            },
            SourceFile {
                path: "unit.c",
                source: "#include \"config.h\"\n#ifdef FREE_THREADED\nstatic int world_stopped(void) { return 0; }\n#else\n#define world_stopped() 1\n#endif\nint check(void) { return world_stopped(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "check",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "world_stopped", "filePath": "unit.c"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:516 — reproduction: the #ifdef branch after it stays possible (cpp)
#[tokio::test]
async fn cpp_macro_and_constructor_calls_516_cpp() {
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_516_cpp",
        files: vec![
            SourceFile {
                path: "config.h",
                source: "#if FREE_THREADED == 0\n#undef FREE_THREADED\n#endif\n",
            },
            SourceFile {
                path: "unit.cpp",
                source: "#include \"config.h\"\n#ifdef FREE_THREADED\nstatic int world_stopped(void) { return 0; }\n#else\n#define world_stopped() 1\n#endif\nint check(void) { return world_stopped(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "check",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "world_stopped", "filePath": "unit.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:537 — control: an #undef on a certain line still makes the flag certainly undefined (c)
#[tokio::test]
async fn cpp_macro_and_constructor_calls_537_c() {
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_537_c",
        files: vec![
            SourceFile {
                path: "config.h",
                source: "#undef TOP_LEVEL\n#if 1\n#undef TRUE_BRANCH\n#endif\n",
            },
            SourceFile {
                path: "unit.c",
                source: "#include \"config.h\"\n#ifdef TOP_LEVEL\n#else\n#define top_hook() 1\n#endif\n#ifdef TRUE_BRANCH\n#else\n#define branch_hook() 1\n#endif\nint use_top(void) { return top_hook(); }\nint use_branch(void) { return branch_hook(); }\n",
            },
            SourceFile {
                path: "decoy.c",
                source: "int top_hook(void) { return 0; }\nint branch_hook(void) { return 0; }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "use_top",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_branch",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:537 — control: an #undef on a certain line still makes the flag certainly undefined (cpp)
#[tokio::test]
async fn cpp_macro_and_constructor_calls_537_cpp() {
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_537_cpp",
        files: vec![
            SourceFile {
                path: "config.h",
                source: "#undef TOP_LEVEL\n#if 1\n#undef TRUE_BRANCH\n#endif\n",
            },
            SourceFile {
                path: "unit.cpp",
                source: "#include \"config.h\"\n#ifdef TOP_LEVEL\n#else\n#define top_hook() 1\n#endif\n#ifdef TRUE_BRANCH\n#else\n#define branch_hook() 1\n#endif\nint use_top(void) { return top_hook(); }\nint use_branch(void) { return branch_hook(); }\n",
            },
            SourceFile {
                path: "decoy.cpp",
                source: "int top_hook(void) { return 0; }\nint branch_hook(void) { return 0; }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "use_top",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_branch",
                expected: json!([
                    {"edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:566 — reproduction: PREFIX(scan)(x) reaches no struct PREFIX, in another file or an included header (c)
#[tokio::test]
async fn cpp_macro_and_constructor_calls_566_c() {
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_566_c",
        files: vec![
            SourceFile {
                path: "unit.c",
                source: "#ifdef TOK_IMPL\n#define PREFIX(ident) ident\n#endif\nint tok(int x) { return PREFIX(scan)(x); }\n",
            },
            SourceFile {
                path: "parser.c",
                source: "struct PREFIX { int x; };\n",
            },
            SourceFile {
                path: "types.h",
                source: "struct WRAP { int x; };\n",
            },
            SourceFile {
                path: "wrapped.c",
                source: "#include \"types.h\"\n#ifdef WRAP_IMPL\n#define WRAP(ident) ident\n#endif\nint wrapped(int x) { return WRAP(scan)(x); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "tok",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "PREFIX"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "wrapped",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "WRAP"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:566 — reproduction: PREFIX(scan)(x) reaches no struct PREFIX, in another file or an included header (cpp)
#[tokio::test]
async fn cpp_macro_and_constructor_calls_566_cpp() {
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_566_cpp",
        files: vec![
            SourceFile {
                path: "unit.cpp",
                source: "#ifdef TOK_IMPL\n#define PREFIX(ident) ident\n#endif\nint tok(int x) { return PREFIX(scan)(x); }\n",
            },
            SourceFile {
                path: "parser.cpp",
                source: "struct PREFIX { int x; };\n",
            },
            SourceFile {
                path: "types.h",
                source: "struct WRAP { int x; };\n",
            },
            SourceFile {
                path: "wrapped.cpp",
                source: "#include \"types.h\"\n#ifdef WRAP_IMPL\n#define WRAP(ident) ident\n#endif\nint wrapped(int x) { return WRAP(scan)(x); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "tok",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "PREFIX"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "wrapped",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "WRAP"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:594 — control: a same-named function is still the callee when the macro may be off (c)
#[tokio::test]
async fn cpp_macro_and_constructor_calls_594_c() {
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_594_c",
        files: vec![
            SourceFile {
                path: "unit.c",
                source: "#ifdef FAST\n#define helper(x) (x)\n#endif\nint run(int x) { return helper(x); }\n",
            },
            SourceFile {
                path: "lib.c",
                source: "int helper(int x) { return x; }\n",
            },
            SourceFile {
                path: "types.c",
                source: "struct helper { int x; };\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "helper", "filePath": "lib.c"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:594 — control: a same-named function is still the callee when the macro may be off (cpp)
#[tokio::test]
async fn cpp_macro_and_constructor_calls_594_cpp() {
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_594_cpp",
        files: vec![
            SourceFile {
                path: "unit.cpp",
                source: "#ifdef FAST\n#define helper(x) (x)\n#endif\nint run(int x) { return helper(x); }\n",
            },
            SourceFile {
                path: "lib.cpp",
                source: "int helper(int x) { return x; }\n",
            },
            SourceFile {
                path: "types.cpp",
                source: "struct helper { int x; };\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "helper", "filePath": "lib.cpp"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cpp-macro-and-constructor-calls.test.ts:607 — control: a C++ type whose name is no macro is still constructed
#[tokio::test]
async fn cpp_macro_and_constructor_calls_607() {
    check_case(Case {
        name: "cpp_macro_and_constructor_calls_607",
        files: vec![
            SourceFile {
                path: "widget.hpp",
                source: "struct Widget { int x; };\n",
            },
            SourceFile {
                path: "use.cpp",
                source: "#include \"widget.hpp\"\nvoid make() { auto w = Widget(1); }\n",
            },
            SourceFile {
                path: "other.cpp",
                source: "#define OTHER(x) (x)\n",
            },
        ],
        queries: vec![Query {
            operation: "callees",
            symbol: "make",
            expected: json!([
                {"edges": [
                    {"symbol": {"name": "Widget", "filePath": "widget.hpp"}},
                ]},
            ]),
            ..Query::default()
        }],
    })
    .await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:42 — single-argument function macro c
#[tokio::test]
async fn resolution_42_c() {
    check_case(Case {
        name: "resolution_42_c",
        files: vec![
            SourceFile {
                path: "main.c",
                source: "#define NATIVE_FN(name) int name(void)\nint helper(void) { return 1; }\nNATIVE_FN(get_version) { return helper(); }\nint use_it(void) { return get_version(); }\nint plain_func(void) { return 42; }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "get_version",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "use_it"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "get_version",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "helper"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_it",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "get_version"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// resolution.test.ts:42 — single-argument function macro cpp
#[tokio::test]
async fn resolution_42_cpp() {
    check_case(Case {
        name: "resolution_42_cpp",
        files: vec![
            SourceFile {
                path: "main.cpp",
                source: "#define NATIVE_FN(name) int name(void)\nint helper(void) { return 1; }\nNATIVE_FN(get_version) { return helper(); }\nint use_it(void) { return get_version(); }\nint plain_func(void) { return 42; }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "get_version",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "use_it"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "get_version",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "helper"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_it",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "get_version"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// extraction.test.ts:54 — getter/setter call edges
// default/wasm backend variants share this one fixture
#[tokio::test]
async fn extraction_54() {
    check_case(Case {
        name: "extraction_54",
        files: vec![
            SourceFile {
                path: "point.ts",
                source: "function read() { return 1; } function write(v: number) {}\nexport class Point { /* é😀 */ get x() { return read(); } set x(v: number) { write(v); }\n  get y() { return read(); }\n  set y(v: number) { write(v); }\n}",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Point::x",
                expected: json!([
                    {"startLine": 2, "edges": [
                        {"symbol": {"name": "read"}},
                    ]},
                    {"startLine": 2, "edges": [
                        {"symbol": {"name": "write"}},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// extraction.test.ts:10753 — attributes a call inside a top-level closure (cobra RunE) to the var, not the file (#693)
#[tokio::test]
async fn extraction_10753() {
    // Checks closure ownership and absence of an unattributed file caller; zg does not expose CodeGraph node.kind.
    check_case(Case {
        name: "extraction_10753",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: "module example.com/proj\n\ngo 1.21\n",
            },
            SourceFile {
                path: "factory.go",
                source: "package main\n\nfunc Wire() error { return nil }\n",
            },
            SourceFile {
                path: "root.go",
                source: "package main\n\ntype Cmd struct{ RunE func() error }\n\nvar rootCmd = &Cmd{\n\tRunE: func() error { return Wire() },\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "Wire",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "rootCmd"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Wire",
                expected: json!([
                    {"edges": [
                        {"symbol": null},
                    ]},
                ]),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cli-definition-grouping.test.ts:87
// cli-definition-grouping.test.ts:114
// cli-definition-grouping.test.ts:128
// cli-definition-grouping.test.ts:145
// cli-definition-grouping.test.ts:151
// cli-definition-grouping.test.ts:162
// cli-definition-grouping.test.ts:172
// cli-definition-grouping.test.ts:182
// cli-definition-grouping.test.ts:194
// cli-definition-grouping.test.ts:204
// cli-definition-grouping.test.ts:215
// cli-definition-grouping.test.ts:254
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn cli_definition_grouping_87() {
    // Ports relationship results through zg engine; CLI/MCP text layout and CodeGraph-only metadata are not asserted.
    check_case(Case {
        name: "cli_definition_grouping_87",
        files: vec![
            SourceFile {
                path: "a/alpha.js",
                source: "export function alpha() { return 1; }\n",
            },
            SourceFile {
                path: "b/beta.js",
                source: "export function beta() { return 1; }\n",
            },
            SourceFile {
                path: "a/svc.js",
                source: "import { alpha } from './alpha.js';\nexport function handle() { return alpha(); }\n",
            },
            SourceFile {
                path: "b/svc.js",
                source: "import { beta } from './beta.js';\nexport function handle() { return beta(); }\n",
            },
            SourceFile {
                path: "a/main.js",
                source: "import { handle } from './svc.js';\nexport function aMain() { return handle(); }\nexport function aEntry() { return aMain(); }\n",
            },
            SourceFile {
                path: "b/main.js",
                source: "import { handle } from './svc.js';\nexport function bMain() { return handle(); }\nexport function bEntry() { return bMain(); }\n",
            },
            SourceFile {
                path: "a/work.js",
                source: "import { shared } from '../shared.js';\nimport { alpha } from './alpha.js';\nexport function work() { shared(); return alpha(); }\n",
            },
            SourceFile {
                path: "b/work.js",
                source: "import { shared } from '../shared.js';\nimport { beta } from './beta.js';\nexport function work() { shared(); return beta(); }\n",
            },
            SourceFile {
                path: "a/work-caller.js",
                source: "import { work } from './work.js';\nexport function aWorker() { work(); }\n",
            },
            SourceFile {
                path: "b/work-caller.js",
                source: "import { work } from './work.js';\nexport function bWorker() { work(); }\n",
            },
            SourceFile {
                path: "shared.js",
                source: "export function shared() {}\n",
            },
            SourceFile {
                path: "both.js",
                source: "import { work as aWork } from './a/work.js';\nimport { work as bWork } from './b/work.js';\nexport function both() { aWork(); bWork(); }\n",
            },
            SourceFile {
                path: "quiet-a.js",
                source: "export function quiet() {}\n",
            },
            SourceFile {
                path: "quiet-b.js",
                source: "import { alpha } from './a/alpha.js';\nexport function quiet() { alpha(); }\nexport function wake() { quiet(); }\n",
            },
            SourceFile {
                path: "scopes.ts",
                source: "function leftOnly() {}\nfunction rightOnly() {}\nexport class Left { run() { leftOnly(); } }\nexport class Right { run() { rightOnly(); } }",
            },
            SourceFile {
                path: "Overloads.java",
                source: "public class Overloads {\n  static String stringIdentity(String value) { return value; }\n  static int intIdentity(int value) { return value; }\n  public static String convert(String value) { return stringIdentity(value); }\n  public static int convert(int value) { return intIdentity(value); }\n  public static void convertCaller() { convert(1); convert(\"value\"); }\n}",
            },
            SourceFile {
                path: "crowd/def-0.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-1.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-2.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-3.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-4.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-5.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-6.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-7.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-8.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-9.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-10.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-11.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-12.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-13.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-14.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-15.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-16.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-17.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-18.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-19.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-20.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-21.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-22.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-23.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-24.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-25.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-26.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-27.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-28.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-29.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-30.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-31.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-32.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-33.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-34.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-35.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-36.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-37.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-38.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-39.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-40.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-41.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-42.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-43.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-44.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-45.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-46.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-47.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-48.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-49.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-50.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-51.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-52.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-53.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
            SourceFile {
                path: "crowd/def-54.js",
                source: "import { shared } from '../shared.js';\nexport function crowded() { shared(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "handle",
                expected: json!([
                    {"filePath": "a/svc.js", "startLine": 2, "edges": [
                        {"symbol": {"filePath": "a/main.js"}},
                    ]},
                    {"filePath": "b/svc.js", "startLine": 2, "edges": [
                        {"symbol": {"filePath": "b/main.js"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "handle",
                expected: json!([
                    {"filePath": "a/svc.js", "startLine": 2, "edges": [
                        {"symbol": {"filePath": "a/main.js"}},
                    ]},
                ]),
                file: Some("a/svc.js"),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "handle",
                expected: json!([
                    {"filePath": "a/svc.js", "startLine": 2, "edges": [
                        {"symbol": {"filePath": "a/main.js"}},
                    ]},
                    {"filePath": "b/svc.js", "startLine": 2, "edges": [
                        {"symbol": {"filePath": "b/main.js"}},
                    ]},
                ]),
                file: Some("**/svc.js"),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Missing.run",
                expected: json!([
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "quiet",
                expected: json!([
                    {"filePath": "quiet-a.js", "edges": [
                    ]},
                    {"filePath": "quiet-b.js", "edges": [
                        {"symbol": {"name": "wake"}},
                    ]},
                ]),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"filePath": "a/work.js", "edges": [
                        {"symbol": {"name": "both"}},
                    ]},
                    {"filePath": "b/work.js", "edges": [
                        {"symbol": {"name": "both"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"filePath": "a/work.js", "truncated": true, "edges": [
                        {},
                    ]},
                    {"filePath": "b/work.js", "truncated": true, "edges": [
                        {},
                    ]},
                ]),
                limit: Some(1),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "handle",
                expected: json!([
                    {"filePath": "a/svc.js", "startLine": 2, "edges": [
                        {"symbol": {"filePath": "a/alpha.js"}},
                    ]},
                    {"filePath": "b/svc.js", "startLine": 2, "edges": [
                        {"symbol": {"filePath": "b/beta.js"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "handle",
                expected: json!([
                    {"filePath": "a/svc.js", "startLine": 2, "edges": [
                        {"symbol": {"filePath": "a/alpha.js"}},
                    ]},
                ]),
                file: Some("a/svc.js"),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "handle",
                expected: json!([
                    {"filePath": "a/svc.js", "startLine": 2, "edges": [
                        {"symbol": {"filePath": "a/alpha.js"}},
                    ]},
                    {"filePath": "b/svc.js", "startLine": 2, "edges": [
                        {"symbol": {"filePath": "b/beta.js"}},
                    ]},
                ]),
                file: Some("**/svc.js"),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Missing.run",
                expected: json!([
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "quiet",
                expected: json!([
                    {"filePath": "quiet-a.js", "edges": [
                    ]},
                    {"filePath": "quiet-b.js", "edges": [
                        {"symbol": {"name": "alpha"}},
                    ]},
                ]),
                short_names: true,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "work",
                expected: json!([
                    {"filePath": "a/work.js", "edges": [
                        {"symbol": {"name": "shared"}},
                    ]},
                    {"filePath": "b/work.js", "edges": [
                        {"symbol": {"name": "shared"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "work",
                expected: json!([
                    {"filePath": "a/work.js", "truncated": true, "edges": [
                        {},
                    ]},
                    {"filePath": "b/work.js", "truncated": true, "edges": [
                        {},
                    ]},
                ]),
                limit: Some(1),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"name": "Left::run", "edges": [
                        {"symbol": {"name": "leftOnly"}},
                    ]},
                    {"name": "Right::run", "edges": [
                        {"symbol": {"name": "rightOnly"}},
                    ]},
                ]),
                file: Some("scopes.ts"),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Left.run",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "leftOnly"}},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "crowded",
                expected: json!([
                    {"filePath": "crowd/def-54.js", "edges": [
                        {"symbol": {"name": "shared"}},
                    ]},
                ]),
                file: Some("crowd/def-54.js"),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// same-name-disambiguation.test.ts:94
// same-name-disambiguation.test.ts:110
// same-name-disambiguation.test.ts:121
// same-name-disambiguation.test.ts:137
#[tokio::test]
async fn same_name_disambiguation_94() {
    // Ports relationship results through zg engine; CLI/MCP text layout and CodeGraph-only metadata are not asserted.
    check_case(Case {
        name: "same_name_disambiguation_94",
        files: vec![
            SourceFile {
                path: "apps/billing/src/users/user.service.ts",
                source: "import { UserRepository } from './user.repository';\nexport class UserService {\n  constructor(private readonly repo: UserRepository) {}\n  findAll(): string[] {\n    return this.repo.load_billing();\n  }\n}",
            },
            SourceFile {
                path: "apps/admin/src/users/user.service.ts",
                source: "import { UserRepository } from './user.repository';\nexport class UserService {\n  constructor(private readonly repo: UserRepository) {}\n  findAll(): string[] {\n    return this.repo.load_admin();\n  }\n}",
            },
            SourceFile {
                path: "apps/billing/src/users/user.repository.ts",
                source: "export class UserRepository {\n  load_billing(): string[] { return []; }\n}\n",
            },
            SourceFile {
                path: "apps/admin/src/users/user.repository.ts",
                source: "export class UserRepository {\n  load_admin(): string[] { return []; }\n}\n",
            },
            SourceFile {
                path: "apps/billing/src/users/user.controller.ts",
                source: "import { UserService } from './user.service';\nexport class UserController {\n  constructor(private readonly users: UserService) {}\n  list(): string[] { return this.users.findAll(); }\n}",
            },
            SourceFile {
                path: "apps/admin/src/users/user.controller.ts",
                source: "import { UserService } from './user.service';\nexport class UserController {\n  constructor(private readonly users: UserService) {}\n  list(): string[] { return this.users.findAll(); }\n}",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "findAll",
                expected: json!([
                    {"filePath": "apps/admin/src/users/user.service.ts", "edges": [
                        {"symbol": {"filePath": "apps/admin/src/users/user.controller.ts"}},
                    ]},
                    {"filePath": "apps/billing/src/users/user.service.ts", "edges": [
                        {"symbol": {"filePath": "apps/billing/src/users/user.controller.ts"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "findAll",
                expected: json!([
                    {"filePath": "apps/billing/src/users/user.service.ts", "edges": [
                        {"symbol": {"filePath": "apps/billing/src/users/user.controller.ts"}},
                    ]},
                ]),
                file: Some("apps/billing/src/users/user.service.ts"),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "list",
                expected: json!([
                    {"edges": [
                    ]},
                    {"edges": [
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// mcp-callers-truncation.test.ts:67
// mcp-callers-truncation.test.ts:76
// mcp-callers-truncation.test.ts:82
// mcp-callers-truncation.test.ts:91
// mcp-callers-truncation.test.ts:98
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the original source fixture and its expected queries together"
)]
async fn mcp_callers_truncation_67() {
    // Ports relationship results through zg engine; CLI/MCP text layout and CodeGraph-only metadata are not asserted.
    check_case(Case {
        name: "mcp_callers_truncation_67",
        files: vec![
            SourceFile {
                path: "src/target.ts",
                source: "export function warm(n: number): number { return n; }\n",
            },
            SourceFile {
                path: "src/callers.ts",
                source: "import { warm } from './target';\nexport function caller0(): number { return warm(0); }\nexport function caller1(): number { return warm(1); }\nexport function caller2(): number { return warm(2); }\nexport function caller3(): number { return warm(3); }\nexport function caller4(): number { return warm(4); }\nexport function caller5(): number { return warm(5); }\nexport function caller6(): number { return warm(6); }\nexport function caller7(): number { return warm(7); }\nexport function caller8(): number { return warm(8); }\nexport function caller9(): number { return warm(9); }\nexport function caller10(): number { return warm(10); }\nexport function caller11(): number { return warm(11); }\nexport function caller12(): number { return warm(12); }\nexport function caller13(): number { return warm(13); }\nexport function caller14(): number { return warm(14); }\nexport function caller15(): number { return warm(15); }\nexport function caller16(): number { return warm(16); }\nexport function caller17(): number { return warm(17); }\nexport function caller18(): number { return warm(18); }\nexport function caller19(): number { return warm(19); }\nexport function caller20(): number { return warm(20); }\nexport function caller21(): number { return warm(21); }\nexport function caller22(): number { return warm(22); }\nexport function caller23(): number { return warm(23); }\nexport function caller24(): number { return warm(24); }\n",
            },
            SourceFile {
                path: "src/hot.ts",
                source: "export function hot(n: number): number { return n; }\n",
            },
            SourceFile {
                path: "src/hot-callers.ts",
                source: "import { hot } from './hot';\nexport function hotCaller0(): number { return hot(0); }\nexport function hotCaller1(): number { return hot(1); }\nexport function hotCaller2(): number { return hot(2); }\nexport function hotCaller3(): number { return hot(3); }\nexport function hotCaller4(): number { return hot(4); }\nexport function hotCaller5(): number { return hot(5); }\nexport function hotCaller6(): number { return hot(6); }\nexport function hotCaller7(): number { return hot(7); }\nexport function hotCaller8(): number { return hot(8); }\nexport function hotCaller9(): number { return hot(9); }\nexport function hotCaller10(): number { return hot(10); }\nexport function hotCaller11(): number { return hot(11); }\nexport function hotCaller12(): number { return hot(12); }\nexport function hotCaller13(): number { return hot(13); }\nexport function hotCaller14(): number { return hot(14); }\nexport function hotCaller15(): number { return hot(15); }\nexport function hotCaller16(): number { return hot(16); }\nexport function hotCaller17(): number { return hot(17); }\nexport function hotCaller18(): number { return hot(18); }\nexport function hotCaller19(): number { return hot(19); }\nexport function hotCaller20(): number { return hot(20); }\nexport function hotCaller21(): number { return hot(21); }\nexport function hotCaller22(): number { return hot(22); }\nexport function hotCaller23(): number { return hot(23); }\nexport function hotCaller24(): number { return hot(24); }\n",
            },
            SourceFile {
                path: "src/other-hot.ts",
                source: "export function hot(n: number): number { return n + 1; }\n",
            },
            SourceFile {
                path: "src/other-hot-callers.ts",
                source: "import { hot } from './other-hot';\nexport function otherHotCaller0(): number { return hot(0); }\nexport function otherHotCaller1(): number { return hot(1); }\nexport function otherHotCaller2(): number { return hot(2); }\nexport function otherHotCaller3(): number { return hot(3); }\nexport function otherHotCaller4(): number { return hot(4); }\nexport function otherHotCaller5(): number { return hot(5); }\nexport function otherHotCaller6(): number { return hot(6); }\nexport function otherHotCaller7(): number { return hot(7); }\nexport function otherHotCaller8(): number { return hot(8); }\nexport function otherHotCaller9(): number { return hot(9); }\nexport function otherHotCaller10(): number { return hot(10); }\nexport function otherHotCaller11(): number { return hot(11); }\nexport function otherHotCaller12(): number { return hot(12); }\nexport function otherHotCaller13(): number { return hot(13); }\nexport function otherHotCaller14(): number { return hot(14); }\nexport function otherHotCaller15(): number { return hot(15); }\nexport function otherHotCaller16(): number { return hot(16); }\nexport function otherHotCaller17(): number { return hot(17); }\nexport function otherHotCaller18(): number { return hot(18); }\nexport function otherHotCaller19(): number { return hot(19); }\nexport function otherHotCaller20(): number { return hot(20); }\nexport function otherHotCaller21(): number { return hot(21); }\nexport function otherHotCaller22(): number { return hot(22); }\nexport function otherHotCaller23(): number { return hot(23); }\nexport function otherHotCaller24(): number { return hot(24); }\n",
            },
            SourceFile {
                path: "src/fan.ts",
                source: "export function helper0(): number { return 0; }\nexport function helper1(): number { return 1; }\nexport function helper2(): number { return 2; }\nexport function helper3(): number { return 3; }\nexport function helper4(): number { return 4; }\nexport function helper5(): number { return 5; }\nexport function helper6(): number { return 6; }\nexport function helper7(): number { return 7; }\nexport function helper8(): number { return 8; }\nexport function helper9(): number { return 9; }\nexport function helper10(): number { return 10; }\nexport function helper11(): number { return 11; }\nexport function helper12(): number { return 12; }\nexport function helper13(): number { return 13; }\nexport function helper14(): number { return 14; }\nexport function helper15(): number { return 15; }\nexport function helper16(): number { return 16; }\nexport function helper17(): number { return 17; }\nexport function helper18(): number { return 18; }\nexport function helper19(): number { return 19; }\nexport function helper20(): number { return 20; }\nexport function helper21(): number { return 21; }\nexport function helper22(): number { return 22; }\nexport function helper23(): number { return 23; }\nexport function helper24(): number { return 24; }\nexport function fanout(): number { return helper0() + helper1() + helper2() + helper3() + helper4() + helper5() + helper6() + helper7() + helper8() + helper9() + helper10() + helper11() + helper12() + helper13() + helper14() + helper15() + helper16() + helper17() + helper18() + helper19() + helper20() + helper21() + helper22() + helper23() + helper24(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "warm",
                expected: json!([
                    {"truncated": true, "edges": [
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                    ]},
                ]),
                endpoint_prefix: Some("caller"),
                limit: Some(20),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "hot",
                expected: json!([
                    {"filePath": "src/hot.ts", "truncated": true, "edges": [
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                    ]},
                    {"filePath": "src/other-hot.ts", "truncated": true, "edges": [
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                    ]},
                ]),
                limit: Some(20),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fanout",
                expected: json!([
                    {"truncated": true, "edges": [
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                        {},
                    ]},
                ]),
                limit: Some(20),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// cli-truncation.test.ts:50
// cli-truncation.test.ts:69
#[tokio::test]
async fn cli_truncation_50() {
    // Ports relationship results through zg engine; CLI/MCP text layout and CodeGraph-only metadata are not asserted.
    check_case(Case {
        name: "cli_truncation_50",
        files: vec![
            SourceFile {
                path: "lib.ts",
                source: "export function target() {}\nexport function helperA() {}\nexport function helperB() {}\nexport function helperC() {}\nexport function source() { helperA(); helperB(); helperC(); }\nexport function TargetHitOne() {}\nexport function TargetHitTwo() {}\nexport function TargetHitThree() {}",
            },
            SourceFile {
                path: "caller-0.ts",
                source: "import { target } from './lib';\nexport function caller0() { target(); }\n",
            },
            SourceFile {
                path: "caller-1.ts",
                source: "import { target } from './lib';\nexport function caller1() { target(); }\n",
            },
            SourceFile {
                path: "caller-2.ts",
                source: "import { target } from './lib';\nexport function caller2() { target(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "target",
                expected: json!([
                    {"truncated": true, "edges": [
                        {},
                        {},
                    ]},
                ]),
                limit: Some(2),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "source",
                expected: json!([
                    {"truncated": true, "edges": [
                        {},
                        {},
                    ]},
                ]),
                limit: Some(2),
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// no-silent-fuzzy-symbol.test.ts:105
// no-silent-fuzzy-symbol.test.ts:113
// no-silent-fuzzy-symbol.test.ts:133
#[tokio::test]
async fn no_silent_fuzzy_symbol_105() {
    // Java source and relationship assertions retained. The separate C# case-sensitivity case is unsupported; textual suggestion wording is not checked.
    check_case(Case {
        name: "no_silent_fuzzy_symbol_105",
        files: vec![
            SourceFile {
                path: "src/a/b/c/D.java",
                source: "package a.b.c;\n\npublic class D {\n    public void e()  { System.out.println(\"e\"); }\n    public void ef() { System.out.println(\"ef\"); }\n}\n",
            },
            SourceFile {
                path: "src/a/b/c/Caller.java",
                source: "package a.b.c;\n\npublic class Caller {\n    public void callsEfOnly() { D d = new D(); d.ef(); }\n    public void alsoCallsEf() { D d = new D(); d.ef(); }\n}\n",
            },
            SourceFile {
                path: "src/fetch.py",
                source: "def fetch():\n    return 1\n\ndef load():\n    return fetch()\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "Calls",
                expected: json!([
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Calls",
                expected: json!([
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "ef",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "callsEfOnly"}},
                        {"symbol": {"name": "alsoCallsEf"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// symbol-lookup.test.ts:391
// symbol-lookup.test.ts:400
#[tokio::test]
async fn symbol_lookup_391() {
    // Ports relationship results through zg engine; CLI/MCP text layout and CodeGraph-only metadata are not asserted.
    check_case(Case {
        name: "symbol_lookup_391",
        files: vec![
            SourceFile {
                path: "client/chart.ts",
                source: "export function group(rows: number[][]): number[][] { return rows; }\n",
            },
            SourceFile {
                path: "client/Editor.tsx",
                source: "import { group } from './chart';\nexport function Editor(r: number[][]) { return group(r); }\n",
            },
            SourceFile {
                path: "pkg/fmtutil/format.py",
                source: "def group(items, size):\n    return items\n",
            },
            SourceFile {
                path: "pkg/planner.py",
                source: "from pkg.fmtutil.format import group\n\ndef plan_a(items): return group(items, 3)\ndef plan_b(items): return group(items, 5)\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "group",
                expected: json!([
                    {"filePath": "client/chart.ts", "edges": [
                        {"symbol": {"filePath": "client/Editor.tsx"}},
                    ]},
                    {"filePath": "pkg/fmtutil/format.py", "edges": [
                        {"symbol": {"filePath": "pkg/planner.py"}},
                    ]},
                ]),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// graph.test.ts:298
#[tokio::test]
async fn graph_298() {
    // Ports relationship results through zg engine; CLI/MCP text layout and CodeGraph-only metadata are not asserted.
    check_case(Case {
        name: "graph_298",
        files: vec![
            SourceFile {
                path: "src/base.ts",
                source: "\nexport class BaseClass {\n  protected value: number;\n\n  constructor(value: number) {\n    this.value = value;\n  }\n\n  getValue(): number {\n    return this.value;\n  }\n}\n\nexport interface Printable {\n  print(): void;\n}\n",
            },
            SourceFile {
                path: "src/derived.ts",
                source: "\nimport { BaseClass, Printable } from './base';\n\nexport class DerivedClass extends BaseClass implements Printable {\n  private name: string;\n\n  constructor(value: number, name: string) {\n    super(value);\n    this.name = name;\n  }\n\n  print(): void {\n    console.log(this.getName(), this.getValue());\n  }\n\n  getName(): string {\n    return this.name;\n  }\n}\n",
            },
            SourceFile {
                path: "src/utils.ts",
                source: "\nexport function formatValue(value: number): string {\n  return value.toFixed(2);\n}\n\nexport function processValue(value: number): number {\n  const formatted = formatValue(value);\n  return parseFloat(formatted);\n}\n\nexport function doubleValue(value: number): number {\n  return value * 2;\n}\n\n// Unused function (dead code)\nfunction unusedHelper(): void {\n  console.log('never called');\n}\n",
            },
            SourceFile {
                path: "src/main.ts",
                source: "\nimport { DerivedClass } from './derived';\nimport { processValue, doubleValue } from './utils';\n\nfunction main(): void {\n  const obj = new DerivedClass(10, 'test');\n  obj.print();\n\n  const result = processValue(doubleValue(obj.getValue()));\n  console.log(result);\n}\n\nexport { main };\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "DerivedClass",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "main"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "main",
                expected: json!([
                    {"edges": [
                        {"symbol": {"name": "DerivedClass"}},
                    ]},
                ]),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// sync.test.ts:590 — creates the cross-file calls edge from the UNCHANGED file after sync
// sync.test.ts:582
#[tokio::test]
async fn sync_590() {
    check_stages(
        &[
            Case {
                name: "sync_590_stage_0",
                files: vec![
                    SourceFile {
                        path: "a.ts",
                        source: "import { greet } from './b';\n\nexport function run(): number {\n  return greet();\n}\n",
                    },
                    SourceFile {
                        path: "b.ts",
                        source: "export function other(): number {\n  return 1;\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "greet",
                        expected: json!([
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "sync_590_stage_1",
                files: vec![
                    SourceFile {
                        path: "a.ts",
                        source: "import { greet } from './b';\n\nexport function run(): number {\n  return greet();\n}\n",
                    },
                    SourceFile {
                        path: "b.ts",
                        source: "export function greet(): number {\n  return 42;\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "greet",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "run"}},
                            ]},
                        ]),
                        short_names: true,
                        check: Check::Contains,
                        ..Query::default()
                    },
                ],
            },
        ],
        false,
    ).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// sync.test.ts:611 — a second sync is a no-op and does not duplicate edges
#[tokio::test]
async fn sync_611() {
    check_stages(
        &[
            Case {
                name: "sync_611_stage_0",
                files: vec![
                    SourceFile {
                        path: "a.ts",
                        source: "import { greet } from './b';\n\nexport function run(): number {\n  return greet();\n}\n",
                    },
                    SourceFile {
                        path: "b.ts",
                        source: "export function other(): number {\n  return 1;\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "greet",
                        expected: json!([
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "sync_611_stage_1",
                files: vec![
                    SourceFile {
                        path: "a.ts",
                        source: "import { greet } from './b';\n\nexport function run(): number {\n  return greet();\n}\n",
                    },
                    SourceFile {
                        path: "b.ts",
                        source: "export function greet(): number {\n  return 42;\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "greet",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "run"}},
                            ]},
                        ]),
                        short_names: true,
                        check: Check::Contains,
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "sync_611_stage_2",
                files: vec![
                    SourceFile {
                        path: "a.ts",
                        source: "import { greet } from './b';\n\nexport function run(): number {\n  return greet();\n}\n",
                    },
                    SourceFile {
                        path: "b.ts",
                        source: "export function greet(): number {\n  return 42;\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "greet",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "run"}},
                            ]},
                        ]),
                        short_names: true,
                        check: Check::Contains,
                        ..Query::default()
                    },
                ],
            },
        ],
        false,
    ).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// sync.test.ts:622 — retries dotted method refs via the name tail when a class gains the method
#[tokio::test]
async fn sync_622() {
    check_stages(
        &[
            Case {
                name: "sync_622_stage_0",
                files: vec![
                    SourceFile {
                        path: "a.ts",
                        source: "import { greet } from './b';\n\nexport function run(): number {\n  return greet();\n}\n",
                    },
                    SourceFile {
                        path: "b.ts",
                        source: "export function other(): number {\n  return 1;\n}\n",
                    },
                    SourceFile {
                        path: "use.ts",
                        source: "import { Helper } from './helper';\n\nexport function useHelper(): number {\n  const h = new Helper();\n  return h.greet();\n}\n",
                    },
                    SourceFile {
                        path: "helper.ts",
                        source: "export class Helper {\n  other(): number {\n    return 1;\n  }\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "greet",
                        expected: json!([
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "sync_622_stage_1",
                files: vec![
                    SourceFile {
                        path: "a.ts",
                        source: "import { greet } from './b';\n\nexport function run(): number {\n  return greet();\n}\n",
                    },
                    SourceFile {
                        path: "b.ts",
                        source: "export function other(): number {\n  return 1;\n}\n",
                    },
                    SourceFile {
                        path: "use.ts",
                        source: "import { Helper } from './helper';\n\nexport function useHelper(): number {\n  const h = new Helper();\n  return h.greet();\n}\n",
                    },
                    SourceFile {
                        path: "helper.ts",
                        source: "export class Helper {\n  other(): number {\n    return 1;\n  }\n  greet(): number {\n    return 42;\n  }\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "greet",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "useHelper"}},
                            ]},
                        ]),
                        short_names: true,
                        check: Check::Contains,
                        ..Query::default()
                    },
                ],
            },
        ],
        false,
    ).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// sync.test.ts:686 — rebinds the unchanged caller when the symbol moves to another file
#[tokio::test]
async fn sync_686() {
    check_stages(
        &[
            Case {
                name: "sync_686_stage_0",
                files: vec![
                    SourceFile {
                        path: "a.ts",
                        source: "export function run(): number {\n  return greet();\n}\n",
                    },
                    SourceFile {
                        path: "b.ts",
                        source: "export function greet(): number {\n  return 42;\n}\n",
                    },
                ],
                queries: vec![Query {
                    operation: "callers",
                    symbol: "greet",
                    expected: json!([
                        {"edges": [
                            {"symbol": {"name": "run"}},
                        ]},
                    ]),
                    short_names: true,
                    check: Check::Contains,
                    ..Query::default()
                }],
            },
            Case {
                name: "sync_686_stage_1",
                files: vec![
                    SourceFile {
                        path: "a.ts",
                        source: "export function run(): number {\n  return greet();\n}\n",
                    },
                    SourceFile {
                        path: "b.ts",
                        source: "export function other(): number {\n  return 1;\n}\n",
                    },
                    SourceFile {
                        path: "d.ts",
                        source: "export function greet(): number {\n  return 42;\n}\n",
                    },
                ],
                queries: vec![Query {
                    operation: "callers",
                    symbol: "greet",
                    expected: json!([
                        {"filePath": "d.ts", "edges": [
                            {"symbol": {"name": "run"}},
                        ]},
                    ]),
                    file: Some("d.ts"),
                    short_names: true,
                    check: Check::Contains,
                    ..Query::default()
                }],
            },
        ],
        true,
    )
    .await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// sync.test.ts:701 — drops the edge on removal and restores it when the symbol returns
#[tokio::test]
async fn sync_701() {
    check_stages(
        &[
            Case {
                name: "sync_701_stage_0",
                files: vec![
                    SourceFile {
                        path: "a.ts",
                        source: "export function run(): number {\n  return greet();\n}\n",
                    },
                    SourceFile {
                        path: "b.ts",
                        source: "export function greet(): number {\n  return 42;\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "greet",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "run"}},
                            ]},
                        ]),
                        short_names: true,
                        check: Check::Contains,
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "sync_701_stage_1",
                files: vec![
                    SourceFile {
                        path: "a.ts",
                        source: "export function run(): number {\n  return greet();\n}\n",
                    },
                    SourceFile {
                        path: "b.ts",
                        source: "export function other(): number {\n  return 1;\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "greet",
                        expected: json!([
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "sync_701_stage_2",
                files: vec![
                    SourceFile {
                        path: "a.ts",
                        source: "export function run(): number {\n  return greet();\n}\n",
                    },
                    SourceFile {
                        path: "b.ts",
                        source: "export function other(): number {\n  return 1;\n}\nexport function greet(): number {\n  return 42;\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "greet",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "run"}},
                            ]},
                        ]),
                        short_names: true,
                        check: Check::Contains,
                        ..Query::default()
                    },
                ],
            },
        ],
        false,
    ).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// sync.test.ts:717 — handles whole-file deletion: parks the ref, then rebinds when the symbol reappears elsewhere
#[tokio::test]
async fn sync_717() {
    check_stages(
        &[
            Case {
                name: "sync_717_stage_0",
                files: vec![
                    SourceFile {
                        path: "a.ts",
                        source: "export function run(): number {\n  return greet();\n}\n",
                    },
                    SourceFile {
                        path: "b.ts",
                        source: "export function greet(): number {\n  return 42;\n}\n",
                    },
                ],
                queries: vec![Query {
                    operation: "callers",
                    symbol: "greet",
                    expected: json!([
                        {"edges": [
                            {"symbol": {"name": "run"}},
                        ]},
                    ]),
                    short_names: true,
                    check: Check::Contains,
                    ..Query::default()
                }],
            },
            Case {
                name: "sync_717_stage_1",
                files: vec![SourceFile {
                    path: "a.ts",
                    source: "export function run(): number {\n  return greet();\n}\n",
                }],
                queries: vec![Query {
                    operation: "callers",
                    symbol: "greet",
                    expected: json!([]),
                    short_names: true,
                    ..Query::default()
                }],
            },
            Case {
                name: "sync_717_stage_2",
                files: vec![
                    SourceFile {
                        path: "a.ts",
                        source: "export function run(): number {\n  return greet();\n}\n",
                    },
                    SourceFile {
                        path: "d.ts",
                        source: "export function greet(): number {\n  return 99;\n}\n",
                    },
                ],
                queries: vec![Query {
                    operation: "callers",
                    symbol: "greet",
                    expected: json!([
                        {"filePath": "d.ts", "edges": [
                            {"symbol": {"name": "run"}},
                        ]},
                    ]),
                    file: Some("d.ts"),
                    short_names: true,
                    check: Check::Contains,
                    ..Query::default()
                }],
            },
        ],
        false,
    )
    .await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// sync.test.ts:796 — preserves incoming cross-file calls edges when the callee file is re-indexed
#[tokio::test]
async fn sync_796() {
    check_stages(
        &[
            Case {
                name: "sync_796_stage_0",
                files: vec![
                    SourceFile {
                        path: "pkg/__init__.py",
                        source: "",
                    },
                    SourceFile {
                        path: "pkg/mod.py",
                        source: "def callee_one(value):\n    \"\"\"First callee — docstring above the second callee so edits here shift its line.\"\"\"\n    return value + 1\n\n\ndef callee_two(value):\n    \"\"\"Second callee, called from the test file via mod.callee_two(...).\"\"\"\n    return value + 2\n",
                    },
                    SourceFile {
                        path: "test/test_callers.py",
                        source: "from pkg import mod\n\n\ndef test_calls_callee_one():\n    assert mod.callee_one(1) == 2\n\n\ndef test_calls_callee_two():\n    assert mod.callee_two(1) == 3\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "callee_one",
                        expected: json!([
                            {"edges": [
                                {},
                            ]},
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                    Query {
                        operation: "callers",
                        symbol: "callee_two",
                        expected: json!([
                            {"edges": [
                                {},
                            ]},
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "sync_796_stage_1",
                files: vec![
                    SourceFile {
                        path: "pkg/__init__.py",
                        source: "",
                    },
                    SourceFile {
                        path: "pkg/mod.py",
                        source: "def callee_one(value):\n    \"\"\"First callee — docstring above the second callee so edits here shift its line.\"\"\"\n    \"\"\"Probe: extra docstring line to shift callee_two's start line by 1.\"\"\"\n    return value + 1\n\n\ndef callee_two(value):\n    \"\"\"Second callee, called from the test file via mod.callee_two(...).\"\"\"\n    return value + 2\n",
                    },
                    SourceFile {
                        path: "test/test_callers.py",
                        source: "from pkg import mod\n\n\ndef test_calls_callee_one():\n    assert mod.callee_one(1) == 2\n\n\ndef test_calls_callee_two():\n    assert mod.callee_two(1) == 3\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "callee_one",
                        expected: json!([
                            {"edges": [
                                {},
                            ]},
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                    Query {
                        operation: "callers",
                        symbol: "callee_two",
                        expected: json!([
                            {"edges": [
                                {},
                            ]},
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                ],
            },
        ],
        false,
    ).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// sync.test.ts:830 — drops incoming edges for a callee that was renamed during re-index
#[tokio::test]
async fn sync_830() {
    check_stages(
        &[
            Case {
                name: "sync_830_stage_0",
                files: vec![
                    SourceFile {
                        path: "pkg/__init__.py",
                        source: "",
                    },
                    SourceFile {
                        path: "pkg/mod.py",
                        source: "def callee_one(value):\n    \"\"\"First callee — docstring above the second callee so edits here shift its line.\"\"\"\n    return value + 1\n\n\ndef callee_two(value):\n    \"\"\"Second callee, called from the test file via mod.callee_two(...).\"\"\"\n    return value + 2\n",
                    },
                    SourceFile {
                        path: "test/test_callers.py",
                        source: "from pkg import mod\n\n\ndef test_calls_callee_one():\n    assert mod.callee_one(1) == 2\n\n\ndef test_calls_callee_two():\n    assert mod.callee_two(1) == 3\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "callee_one",
                        expected: json!([
                            {"edges": [
                                {},
                            ]},
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "sync_830_stage_1",
                files: vec![
                    SourceFile {
                        path: "pkg/__init__.py",
                        source: "",
                    },
                    SourceFile {
                        path: "pkg/mod.py",
                        source: "def callee_one_renamed(value):\n    \"\"\"Renamed callee — the old edge targeting callee_one must not be restored.\"\"\"\n    return value + 1\n\n\ndef callee_two(value):\n    \"\"\"Second callee, called from the test file via mod.callee_two(...).\"\"\"\n    return value + 2\n",
                    },
                    SourceFile {
                        path: "test/test_callers.py",
                        source: "from pkg import mod\n\n\ndef test_calls_callee_one():\n    assert mod.callee_one(1) == 2\n\n\ndef test_calls_callee_two():\n    assert mod.callee_two(1) == 3\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "callee_one",
                        expected: json!([
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                    Query {
                        operation: "callers",
                        symbol: "callee_one_renamed",
                        expected: json!([
                            {"edges": [
                            ]},
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                ],
            },
        ],
        false,
    ).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// sync.test.ts:951 — same-named methods after body
#[tokio::test]
async fn sync_951_body() {
    check_stages(
        &[
            Case {
                name: "sync_951_body_before",
                files: vec![
                    SourceFile {
                        path: "ops.ts",
                        source: "export class Foo {\n  execute(): number {\n    return 1;\n  }\n}\n\nexport class Bar {\n  execute(): number {\n    return 2;\n  }\n}\n",
                    },
                    SourceFile {
                        path: "callers.ts",
                        source: "import { Foo, Bar } from './ops';\n\nexport function runFoo(): number {\n  const foo = new Foo();\n  return foo.execute();\n}\n\nexport function runBar(): number {\n  const bar = new Bar();\n  return bar.execute();\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "Foo::execute",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "runFoo"}},
                            ]},
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                    Query {
                        operation: "callers",
                        symbol: "Bar::execute",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "runBar"}},
                            ]},
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "sync_951_body_after",
                files: vec![
                    SourceFile {
                        path: "ops.ts",
                        source: "export class Foo {\n  execute(): number {\n    return 10;\n  }\n}\n\nexport class Bar {\n  execute(): number {\n    return 2;\n  }\n}\n",
                    },
                    SourceFile {
                        path: "callers.ts",
                        source: "import { Foo, Bar } from './ops';\n\nexport function runFoo(): number {\n  const foo = new Foo();\n  return foo.execute();\n}\n\nexport function runBar(): number {\n  const bar = new Bar();\n  return bar.execute();\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "Foo::execute",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "runFoo"}},
                            ]},
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                    Query {
                        operation: "callers",
                        symbol: "Bar::execute",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "runBar"}},
                            ]},
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                ],
            },
        ],
        true,
    ).await;
}

// https://github.com/colbymchenry/codegraph/tree/dc4ccf6b1468bb77eed3e765f3200ae16a6834bd/__tests__/
// sync.test.ts:951 — same-named methods after line_shift
#[tokio::test]
async fn sync_951_line_shift() {
    check_stages(
        &[
            Case {
                name: "sync_951_line_shift_before",
                files: vec![
                    SourceFile {
                        path: "ops.ts",
                        source: "export class Foo {\n  execute(): number {\n    return 1;\n  }\n}\n\nexport class Bar {\n  execute(): number {\n    return 2;\n  }\n}\n",
                    },
                    SourceFile {
                        path: "callers.ts",
                        source: "import { Foo, Bar } from './ops';\n\nexport function runFoo(): number {\n  const foo = new Foo();\n  return foo.execute();\n}\n\nexport function runBar(): number {\n  const bar = new Bar();\n  return bar.execute();\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "Foo::execute",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "runFoo"}},
                            ]},
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                    Query {
                        operation: "callers",
                        symbol: "Bar::execute",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "runBar"}},
                            ]},
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "sync_951_line_shift_after",
                files: vec![
                    SourceFile {
                        path: "ops.ts",
                        source: "export class Foo {\n  execute(): number {\n    const n = 1;\n    return n;\n  }\n}\n\nexport class Bar {\n  execute(): number {\n    return 2;\n  }\n}\n",
                    },
                    SourceFile {
                        path: "callers.ts",
                        source: "import { Foo, Bar } from './ops';\n\nexport function runFoo(): number {\n  const foo = new Foo();\n  return foo.execute();\n}\n\nexport function runBar(): number {\n  const bar = new Bar();\n  return bar.execute();\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callers",
                        symbol: "Foo::execute",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "runFoo"}},
                            ]},
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                    Query {
                        operation: "callers",
                        symbol: "Bar::execute",
                        expected: json!([
                            {"edges": [
                                {"symbol": {"name": "runBar"}},
                            ]},
                        ]),
                        short_names: true,
                        ..Query::default()
                    },
                ],
            },
        ],
        true,
    ).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/integration/full-pipeline.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_forward_chain() {
    check_case(Case {
        name: "basic_forward_chain",
        files: vec![
            SourceFile {
                path: "chain.ts",
                source: "function entry() {\n  middle();\n}\nfunction middle() {\n  leaf();\n}\nfunction leaf() {}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "entry",
                expected: json!([
                    {"name": "entry", "filePath": "chain.ts", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "entry",
                expected: json!([
                    {"name": "entry", "filePath": "chain.ts", "startLine": 1, "edges": [
                        {"symbol": {"name": "middle", "filePath": "chain.ts", "startLine": 4}, "line": 2},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "leaf",
                expected: json!([
                    {"name": "leaf", "filePath": "chain.ts", "startLine": 7, "edges": [
                        {"symbol": {"name": "middle", "filePath": "chain.ts", "startLine": 4}, "line": 5},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "leaf",
                expected: json!([
                    {"name": "leaf", "filePath": "chain.ts", "startLine": 7, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "middle",
                expected: json!([
                    {"name": "middle", "filePath": "chain.ts", "startLine": 4, "edges": [
                        {"symbol": {"name": "entry", "filePath": "chain.ts", "startLine": 1}, "line": 2},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "middle",
                expected: json!([
                    {"name": "middle", "filePath": "chain.ts", "startLine": 4, "edges": [
                        {"symbol": {"name": "leaf", "filePath": "chain.ts", "startLine": 7}, "line": 5},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/graph.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_repeated_calls() {
    check_case(Case {
        name: "basic_repeated_calls",
        files: vec![
            SourceFile {
                path: "repeat.rs",
                source: "fn target() {}\nfn invoke() {\n    target(); target();\n    invoke();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "repeat.rs", "startLine": 2, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "repeat.rs", "startLine": 2}, "line": 4},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "repeat.rs", "startLine": 2, "edges": [
                        {"symbol": {"name": "target", "filePath": "repeat.rs", "startLine": 1}, "line": [3]},
                        {"symbol": {"name": "invoke", "filePath": "repeat.rs", "startLine": 2}, "line": 4},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "target",
                expected: json!([
                    {"name": "target", "filePath": "repeat.rs", "startLine": 1, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "repeat.rs", "startLine": 2}, "line": [3]},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "target",
                expected: json!([
                    {"name": "target", "filePath": "repeat.rs", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/cli-definition-grouping.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_same_name_definitions() {
    check_case(Case {
        name: "basic_same_name_definitions",
        files: vec![
            SourceFile {
                path: "left.js",
                source: "function target() {}\nfunction invoke() {\n  target();\n}\n",
            },
            SourceFile {
                path: "right.js",
                source: "function target() {}\nfunction invoke() {\n  target();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "left.js", "startLine": 2, "edges": [
                    ]},
                    {"name": "invoke", "filePath": "right.js", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "left.js", "startLine": 2, "edges": [
                        {"symbol": {"name": "target", "filePath": "left.js", "startLine": 1}, "line": 3},
                    ]},
                    {"name": "invoke", "filePath": "right.js", "startLine": 2, "edges": [
                        {"symbol": {"name": "target", "filePath": "right.js", "startLine": 1}, "line": 3},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "target",
                expected: json!([
                    {"name": "target", "filePath": "left.js", "startLine": 1, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "left.js", "startLine": 2}, "line": 3},
                    ]},
                    {"name": "target", "filePath": "right.js", "startLine": 1, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "right.js", "startLine": 2}, "line": 3},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "target",
                expected: json!([
                    {"name": "target", "filePath": "left.js", "startLine": 1, "edges": [
                    ]},
                    {"name": "target", "filePath": "right.js", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/call-receiver-no-fabrication.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_python_unknown_receiver() {
    check_case(Case {
        name: "basic_python_unknown_receiver",
        files: vec![
            SourceFile {
                path: "receiver.py",
                source: "def flush():\n    pass\ndef build():\n    return {}\ndef invoke():\n    build().flush()\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "build",
                expected: json!([
                    {"name": "build", "filePath": "receiver.py", "startLine": 3, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "receiver.py", "startLine": 5}, "line": 6},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "build",
                expected: json!([
                    {"name": "build", "filePath": "receiver.py", "startLine": 3, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "flush",
                expected: json!([
                    {"name": "flush", "filePath": "receiver.py", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "flush",
                expected: json!([
                    {"name": "flush", "filePath": "receiver.py", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "receiver.py", "startLine": 5, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "receiver.py", "startLine": 5, "edges": [
                        {"symbol": {"name": "build", "filePath": "receiver.py", "startLine": 3}, "line": 6},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/call-receiver-no-fabrication.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_javascript_unknown_receiver() {
    check_case(Case {
        name: "basic_javascript_unknown_receiver",
        files: vec![
            SourceFile {
                path: "receiver.js",
                source: "function flush() {}\nfunction build() { return {}; }\nfunction invoke() {\n  build().flush();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "build",
                expected: json!([
                    {"name": "build", "filePath": "receiver.js", "startLine": 2, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "receiver.js", "startLine": 3}, "line": 4},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "build",
                expected: json!([
                    {"name": "build", "filePath": "receiver.js", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "flush",
                expected: json!([
                    {"name": "flush", "filePath": "receiver.js", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "flush",
                expected: json!([
                    {"name": "flush", "filePath": "receiver.js", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "receiver.js", "startLine": 3, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "receiver.js", "startLine": 3, "edges": [
                        {"symbol": {"name": "build", "filePath": "receiver.js", "startLine": 2}, "line": 4},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/bare-call-no-method.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_bare_call_not_method() {
    check_case(Case {
        name: "basic_bare_call_not_method",
        files: vec![SourceFile {
            path: "scope.ts",
            source: "class Worker {\n  flush() {}\n}\nfunction invoke() {\n  flush();\n}\n",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "Worker::flush",
                expected: json!([
                    {"name": "Worker::flush", "filePath": "scope.ts", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Worker::flush",
                expected: json!([
                    {"name": "Worker::flush", "filePath": "scope.ts", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "scope.ts", "startLine": 4, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "scope.ts", "startLine": 4, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    })
    .await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/cli-definition-grouping.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_typescript_import_disambiguation() {
    check_case(Case {
        name: "basic_typescript_import_disambiguation",
        files: vec![
            SourceFile {
                path: "chosen.ts",
                source: "export function target() {}\n",
            },
            SourceFile {
                path: "other.ts",
                source: "export function target() {}\n",
            },
            SourceFile {
                path: "consumer.ts",
                source: "import { target } from './chosen';\nexport function invoke() {\n  target();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "consumer.ts", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "consumer.ts", "startLine": 2, "edges": [
                        {"symbol": {"name": "target", "filePath": "chosen.ts", "startLine": 1}, "line": 3},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "target",
                expected: json!([
                    {"name": "target", "filePath": "chosen.ts", "startLine": 1, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "consumer.ts", "startLine": 2}, "line": 3},
                    ]},
                    {"name": "target", "filePath": "other.ts", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "target",
                expected: json!([
                    {"name": "target", "filePath": "chosen.ts", "startLine": 1, "edges": [
                    ]},
                    {"name": "target", "filePath": "other.ts", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/resolution.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_rust_cross_file_alias() {
    check_case(Case {
        name: "basic_rust_cross_file_alias",
        files: vec![
            SourceFile {
                path: "target.rs",
                source: "pub fn send() {}\n",
            },
            SourceFile {
                path: "lib.rs",
                source: "mod target;\nuse target::send as dispatch;\npub fn invoke() {\n    dispatch();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "lib.rs", "startLine": 3, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "lib.rs", "startLine": 3, "edges": [
                        {"symbol": {"name": "send", "filePath": "target.rs", "startLine": 1}, "line": 4},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "target.rs", "startLine": 1, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "lib.rs", "startLine": 3}, "line": 4},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "target.rs", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/resolution.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_python_import_alias() {
    check_case(Case {
        name: "basic_python_import_alias",
        files: vec![
            SourceFile {
                path: "worker.py",
                source: "def send():\n    pass\n",
            },
            SourceFile {
                path: "client.py",
                source: "from worker import send as dispatch\ndef invoke():\n    dispatch()\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "client.py", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "client.py", "startLine": 2, "edges": [
                        {"symbol": {"name": "send", "filePath": "worker.py", "startLine": 1}, "line": 3},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "worker.py", "startLine": 1, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "client.py", "startLine": 2}, "line": 3},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "worker.py", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/c-static-call-rescue.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_c_header_call() {
    check_case(Case {
        name: "basic_c_header_call",
        files: vec![
            SourceFile {
                path: "worker.h",
                source: "static inline void send(void) {}\n",
            },
            SourceFile {
                path: "client.c",
                source: "#include \"worker.h\"\nvoid invoke(void) {\n    send();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "client.c", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "client.c", "startLine": 2, "edges": [
                        {"symbol": {"name": "send", "filePath": "worker.h", "startLine": 1}, "line": 3},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "worker.h", "startLine": 1, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "client.c", "startLine": 2}, "line": 3},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "worker.h", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/alias-binding-resolution.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_renamed_export() {
    check_case(Case {
        name: "basic_renamed_export",
        files: vec![
            SourceFile {
                path: "worker.ts",
                source: "function send() {}\nexport { send as dispatch };\n",
            },
            SourceFile {
                path: "client.ts",
                source: "import { dispatch } from './worker';\nexport function invoke() {\n  dispatch();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "client.ts", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "client.ts", "startLine": 2, "edges": [
                        {"symbol": {"name": "send", "filePath": "worker.ts", "startLine": 1}, "line": 3},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "worker.ts", "startLine": 1, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "client.ts", "startLine": 2}, "line": 3},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "worker.ts", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/alias-binding-resolution.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_exported_alias_binding() {
    check_case(Case {
        name: "basic_exported_alias_binding",
        files: vec![
            SourceFile {
                path: "worker.ts",
                source: "export function send() {}\nexport const dispatch = send;\n",
            },
            SourceFile {
                path: "client.ts",
                source: "import { dispatch } from './worker';\nexport function invoke() {\n  dispatch();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "client.ts", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "client.ts", "startLine": 2, "edges": [
                        {"symbol": {"name": "send", "filePath": "worker.ts", "startLine": 1}, "line": 3},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "worker.ts", "startLine": 1, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "client.ts", "startLine": 2}, "line": 3},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "worker.ts", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/alias-binding-resolution.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_local_function_alias() {
    check_case(Case {
        name: "basic_local_function_alias",
        files: vec![
            SourceFile {
                path: "alias.ts",
                source: "function send() {}\nconst dispatch = send;\nfunction invoke() {\n  dispatch();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "alias.ts", "startLine": 3, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "alias.ts", "startLine": 3, "edges": [
                        {"symbol": {"name": "send", "filePath": "alias.ts", "startLine": 1}, "line": 4},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "alias.ts", "startLine": 1, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "alias.ts", "startLine": 3}, "line": 4},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "alias.ts", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/alias-binding-resolution.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_object_property_alias() {
    check_case(Case {
        name: "basic_object_property_alias",
        files: vec![
            SourceFile {
                path: "worker.ts",
                source: "export function send() {}\nexport const service = { dispatch: send };\n",
            },
            SourceFile {
                path: "client.ts",
                source: "import { service } from './worker';\nexport function invoke() {\n  service.dispatch();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "client.ts", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "client.ts", "startLine": 2, "edges": [
                        {"symbol": {"name": "send", "filePath": "worker.ts", "startLine": 1}, "line": 3},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "worker.ts", "startLine": 1, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "client.ts", "startLine": 2}, "line": 3},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "worker.ts", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/alias-binding-resolution.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_import_parameter_shadow() {
    check_case(Case {
        name: "basic_import_parameter_shadow",
        files: vec![
            SourceFile {
                path: "worker.ts",
                source: "export function send() {}\n",
            },
            SourceFile {
                path: "client.ts",
                source: "import { send as dispatch } from './worker';\nexport function invoke(dispatch: () => void) {\n  dispatch();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "client.ts", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "client.ts", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "worker.ts", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "worker.ts", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/cpp-free-function-overloads.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_cpp_overload_arity() {
    check_case(Case {
        name: "basic_cpp_overload_arity",
        files: vec![
            SourceFile {
                path: "arity.cpp",
                source: "void send(int value) {}\nvoid send(int value, int count) {}\nvoid invoke() {\n    send(1, 2);\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "arity.cpp", "startLine": 3, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "arity.cpp", "startLine": 3, "edges": [
                        {"symbol": {"name": "send", "filePath": "arity.cpp", "startLine": 2}, "line": 4},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "arity.cpp", "startLine": 1, "edges": [
                    ]},
                    {"name": "send", "filePath": "arity.cpp", "startLine": 2, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "arity.cpp", "startLine": 3}, "line": 4},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "arity.cpp", "startLine": 1, "edges": [
                    ]},
                    {"name": "send", "filePath": "arity.cpp", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/cpp-free-function-overloads.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_cpp_overload_type() {
    check_case(Case {
        name: "basic_cpp_overload_type",
        files: vec![
            SourceFile {
                path: "types.cpp",
                source: "void send(int value) {}\nvoid send(const char* value) {}\nvoid invoke() {\n    send(\"hello\");\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "types.cpp", "startLine": 3, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "types.cpp", "startLine": 3, "edges": [
                        {"symbol": {"name": "send", "filePath": "types.cpp", "startLine": 2}, "line": 4},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "types.cpp", "startLine": 1, "edges": [
                    ]},
                    {"name": "send", "filePath": "types.cpp", "startLine": 2, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "types.cpp", "startLine": 3}, "line": 4},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "types.cpp", "startLine": 1, "edges": [
                    ]},
                    {"name": "send", "filePath": "types.cpp", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/cpp-free-function-overloads.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_cpp_qualified_overload() {
    check_case(Case {
        name: "basic_cpp_qualified_overload",
        files: vec![
            SourceFile {
                path: "qualified.cpp",
                source: "namespace wire {\nvoid send(int value) {}\nvoid send(const char* value) {}\n}\nnamespace other {\nvoid send(const char* value) {}\n}\nvoid invoke() {\n    wire::send(\"hello\");\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "qualified.cpp", "startLine": 8, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "qualified.cpp", "startLine": 8, "edges": [
                        {"symbol": {"name": "wire::send", "filePath": "qualified.cpp", "startLine": 3}, "line": 9},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "other::send",
                expected: json!([
                    {"name": "other::send", "filePath": "qualified.cpp", "startLine": 6, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "other::send",
                expected: json!([
                    {"name": "other::send", "filePath": "qualified.cpp", "startLine": 6, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "wire::send",
                expected: json!([
                    {"name": "wire::send", "filePath": "qualified.cpp", "startLine": 2, "edges": [
                    ]},
                    {"name": "wire::send", "filePath": "qualified.cpp", "startLine": 3, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "qualified.cpp", "startLine": 8}, "line": 9},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "wire::send",
                expected: json!([
                    {"name": "wire::send", "filePath": "qualified.cpp", "startLine": 2, "edges": [
                    ]},
                    {"name": "wire::send", "filePath": "qualified.cpp", "startLine": 3, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/cpp-free-function-overloads.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_cpp_template_overload() {
    check_case(Case {
        name: "basic_cpp_template_overload",
        files: vec![
            SourceFile {
                path: "generic.cpp",
                source: "template <typename T> void send(T value) {}\nvoid send(int value) {}\nvoid number() {\n    send(1);\n}\nvoid text() {\n    send(\"hello\");\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "number",
                expected: json!([
                    {"name": "number", "filePath": "generic.cpp", "startLine": 3, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "number",
                expected: json!([
                    {"name": "number", "filePath": "generic.cpp", "startLine": 3, "edges": [
                        {"symbol": {"name": "send", "filePath": "generic.cpp", "startLine": 2}, "line": 4},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "generic.cpp", "startLine": 1, "edges": [
                        {"symbol": {"name": "text", "filePath": "generic.cpp", "startLine": 6}, "line": 7},
                    ]},
                    {"name": "send", "filePath": "generic.cpp", "startLine": 2, "edges": [
                        {"symbol": {"name": "number", "filePath": "generic.cpp", "startLine": 3}, "line": 4},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "generic.cpp", "startLine": 1, "edges": [
                    ]},
                    {"name": "send", "filePath": "generic.cpp", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "text",
                expected: json!([
                    {"name": "text", "filePath": "generic.cpp", "startLine": 6, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "text",
                expected: json!([
                    {"name": "text", "filePath": "generic.cpp", "startLine": 6, "edges": [
                        {"symbol": {"name": "send", "filePath": "generic.cpp", "startLine": 1}, "line": 7},
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// __tests__/cpp-free-function-overloads.test.ts (previously adapted small fixture; not a verbatim upstream copy)
#[tokio::test]
async fn basic_cpp_translation_unit_visibility() {
    check_case(Case {
        name: "basic_cpp_translation_unit_visibility",
        files: vec![
            SourceFile {
                path: "worker.h",
                source: "inline void send(long value) {}\n",
            },
            SourceFile {
                path: "private.cpp",
                source: "static void send(int value) {}\n",
            },
            SourceFile {
                path: "client.cpp",
                source: "#include \"worker.h\"\nvoid invoke() {\n    send(1);\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "client.cpp", "startLine": 2, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "invoke",
                expected: json!([
                    {"name": "invoke", "filePath": "client.cpp", "startLine": 2, "edges": [
                        {"symbol": {"name": "send", "filePath": "worker.h", "startLine": 1}, "line": 3},
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "worker.h", "startLine": 1, "edges": [
                        {"symbol": {"name": "invoke", "filePath": "client.cpp", "startLine": 2}, "line": 3},
                    ]},
                    {"name": "send", "filePath": "private.cpp", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "send",
                expected: json!([
                    {"name": "send", "filePath": "worker.h", "startLine": 1, "edges": [
                    ]},
                    {"name": "send", "filePath": "private.cpp", "startLine": 1, "edges": [
                    ]},
                ]),
                ..Query::default()
            },
        ],
    }).await;
}

// Restored adapted fixture; not a verbatim upstream test.
// sync.test.ts (previously adapted small fixture)
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the source stages and their expected queries together"
)]
async fn basic_changed_calls_match_expected_graph_and_full_rebuild() {
    check_stages(
        &[
            Case {
                name: "incremental_calls",
                files: vec![
                    SourceFile {
                        path: "edit.rs",
                        source: "fn old_target() {}\nfn new_target() {}\nfn invoke() {\n    old_target();\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callees",
                        symbol: "invoke",
                        expected: json!([
                            {"name": "invoke", "edges": [
                                {"symbol": {"name": "old_target", "filePath": "edit.rs", "startLine": 1}, "line": 4},
                            ]},
                        ]),
                        ..Query::default()
                    },
                    Query {
                        operation: "callers",
                        symbol: "old_target",
                        expected: json!([
                            {"name": "old_target", "filePath": "edit.rs", "startLine": 1, "edges": [
                                {"symbol": {"name": "invoke", "filePath": "edit.rs", "startLine": 3}, "line": 4},
                            ]},
                        ]),
                        ..Query::default()
                    },
                    Query {
                        operation: "callers",
                        symbol: "new_target",
                        expected: json!([
                            {"name": "new_target", "filePath": "edit.rs", "startLine": 2, "edges": [
                            ]},
                        ]),
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "incremental_calls",
                files: vec![
                    SourceFile {
                        path: "edit.rs",
                        source: "fn old_target() {}\nfn new_target() {}\nfn invoke() {\n    new_target(); new_target();\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callees",
                        symbol: "invoke",
                        expected: json!([
                            {"name": "invoke", "edges": [
                                {"symbol": {"name": "new_target", "filePath": "edit.rs", "startLine": 2}, "line": [4]},
                            ]},
                        ]),
                        ..Query::default()
                    },
                    Query {
                        operation: "callers",
                        symbol: "old_target",
                        expected: json!([
                            {"name": "old_target", "filePath": "edit.rs", "startLine": 1, "edges": [
                            ]},
                        ]),
                        ..Query::default()
                    },
                    Query {
                        operation: "callers",
                        symbol: "new_target",
                        expected: json!([
                            {"name": "new_target", "filePath": "edit.rs", "startLine": 2, "edges": [
                                {"symbol": {"name": "invoke", "filePath": "edit.rs", "startLine": 3}, "line": [4]},
                            ]},
                        ]),
                        ..Query::default()
                    },
                ],
            },
            Case {
                name: "incremental_calls",
                files: vec![
                    SourceFile {
                        path: "edit.rs",
                        source: "fn old_target() {}\nfn new_target() {}\nfn invoke() {\n\n}\n",
                    },
                ],
                queries: vec![
                    Query {
                        operation: "callees",
                        symbol: "invoke",
                        expected: json!([
                            {"name": "invoke", "edges": [
                            ]},
                        ]),
                        ..Query::default()
                    },
                    Query {
                        operation: "callers",
                        symbol: "old_target",
                        expected: json!([
                            {"name": "old_target", "filePath": "edit.rs", "startLine": 1, "edges": [
                            ]},
                        ]),
                        ..Query::default()
                    },
                    Query {
                        operation: "callers",
                        symbol: "new_target",
                        expected: json!([
                            {"name": "new_target", "filePath": "edit.rs", "startLine": 2, "edges": [
                            ]},
                        ]),
                        ..Query::default()
                    },
                ],
            },
        ],
        true,
    ).await;
}
