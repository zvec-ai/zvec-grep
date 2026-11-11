//! Shared source → indexing → relationship-query test runner.

use crate::support::{EmbeddingServer, configure_remote_model, index_options};
use serde_json::{Value, json};
use std::{fs, path::Path};
use zg_engine::{ZvecGrep, api::relationships::RelationshipOptions};

static WORKSPACES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub(super) struct Case {
    pub name: &'static str,
    pub files: Vec<SourceFile>,
    pub queries: Vec<Query>,
}

pub(super) struct SourceFile {
    pub path: &'static str,
    pub source: &'static str,
}

#[derive(Default)]
pub(super) struct Query {
    pub operation: &'static str,
    pub symbol: &'static str,
    pub expected: Value,
    // Keep upstream toContain/not.toContain semantics where its list is partial.
    pub check: Check,
    pub file: Option<&'static str>,
    pub endpoint_suffix: Option<&'static str>,
    pub endpoint_prefix: Option<&'static str>,
    pub edge_filters: Vec<(&'static str, &'static str, bool)>,
    pub limit: Option<usize>,
    pub definition_line: Option<usize>,
    pub short_names: bool,
    // Used only where the upstream assertion explicitly constructs a Set.
    pub distinct_symbols: bool,
    // An upstream negative assertion may apply to every same-named definition.
    pub any_definition: bool,
}

#[derive(Default, Clone, Copy)]
pub(super) enum Check {
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

pub(super) async fn check_case(case: Case) {
    check_stages(&[case], false).await;
}

pub(super) async fn check_stages(stages: &[Case], rebuild_each: bool) {
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
