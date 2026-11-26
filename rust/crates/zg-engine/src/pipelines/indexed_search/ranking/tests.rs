// Neutral rules must preserve exact scores and configured factors.
#![allow(clippy::float_cmp)]

use super::*;
use crate::{
    api::context::{
        options::ContextRouteMode,
        result::{MatchedBy, SearchFinalTrace, SearchFusionTrace, SearchHitTrace},
    },
    domain::{
        CodeMetadata, Content, Entity, EntityFragment, EntityId, FileIndexStatus, FileRecord,
        FileSnapshot, FragmentId, Range, SourcePath, SymbolType,
    },
};

fn code(kind: SymbolType, name: &str, scope: Option<&str>) -> CodeMetadata {
    CodeMetadata {
        symbol_type: Some(kind),
        symbol_name: Some(name.into()),
        scope: scope.map(str::to_owned),
        signature: None,
        documentation: None,
        visibility: None,
        language: None,
        arity: None,
        is_exported: false,
        is_declaration: false,
        package: None,
        has_internal_linkage: false,
        declares_type: false,
        alias_of: None,
        object_members: Vec::new(),
        overwritten_by: None,
        exported_as: Vec::new(),
    }
}

pub(super) fn hit(
    id: &str,
    file_id: u32,
    path: &str,
    score: f64,
    rank: usize,
    metadata: Option<CodeMetadata>,
) -> SearchHit {
    fixture_hit(
        id,
        file_id,
        path,
        score,
        rank,
        Content::Code("source".into()),
        metadata,
    )
}

fn document(id: &str, file_id: u32, path: &str, score: f64, rank: usize) -> SearchHit {
    fixture_hit(
        id,
        file_id,
        path,
        score,
        rank,
        Content::Text("source".into()),
        None,
    )
}

fn fixture_hit(
    id: &str,
    file_id: u32,
    path: &str,
    score: f64,
    rank: usize,
    content: Content,
    metadata: Option<CodeMetadata>,
) -> SearchHit {
    let id = EntityId::from_string(id.to_owned());
    let fragment = EntityFragment {
        id: FragmentId::new(&id, 0),
        range: Range::Full,
    };
    SearchHit {
        entity: Entity::new(
            id,
            FileId::new(file_id),
            Range::Full,
            content,
            metadata.map(EntityMetadata::Code),
            vec![fragment],
        )
        .expect("entity"),
        file: FileRecord::new(
            FileId::new(file_id),
            SourcePath::new(path).expect("path"),
            FileSnapshot {
                size_bytes: 6,
                modified_epoch_ms: Some(0),
                content_hash: Some(crate::utils::sha256_hex(b"source")),
            },
            FileIndexStatus::Indexed {
                indexed_epoch_ms: 0,
                entity_count: 1,
            },
        )
        .expect("file"),
        evidence: vec![],
        rank,
        score,
        matched_by: MatchedBy::Fts,
        trace: Some(SearchHitTrace {
            recall: vec![],
            fusion: SearchFusionTrace {
                rank,
                score,
                forced: false,
            },
            ranking: None,
            final_selection: SearchFinalTrace {
                returned_by_limit: true,
                cutoff_rank: 10,
            },
        }),
    }
}

fn run(query: &str, hits: Vec<SearchHit>, filter: &QueryFilter, limit: usize) -> Vec<SearchHit> {
    rank(
        hits,
        &[ResolvedSearchRoute {
            id: "fts".into(),
            mode: ContextRouteMode::Fts,
            query: query.into(),
        }],
        filter,
        1.0,
        limit,
        None,
    )
}

#[test]
fn documents_keep_fusion_order_and_scores_across_paths_and_repeated_files() {
    let candidates = vec![
        document("intro", 1, "tests/guide.md", 0.016, 1),
        document("details", 1, "tests/guide.md", 0.015, 2),
        document("notes", 2, "widget.txt", 0.014, 3),
    ];
    let out = run("widget", candidates, &QueryFilter::default(), 2);
    assert_eq!(out.len(), 2);
    for (hit, (id, rank, score)) in out.iter().zip([("intro", 1, 0.016), ("details", 2, 0.015)]) {
        assert_eq!(hit.entity.id().as_str(), id);
        assert_eq!((hit.rank, hit.score), (rank, score));
        let trace = hit.trace.as_ref().expect("trace");
        assert_eq!((trace.fusion.rank, trace.fusion.score), (rank, score));
        assert!(trace.ranking.is_none());
        assert!(trace.final_selection.returned_by_limit);
        assert_eq!(trace.final_selection.cutoff_rank, 2);
    }
}

#[test]
fn mixed_results_promote_a_definition_from_below_cutoff_into_a_code_position() {
    let candidates = vec![
        document("guide", 1, "tests/guide.md", 1.0, 1),
        hit("usage", 2, "src/client.rs", 0.95, 2, None),
        document("notes", 3, "notes.txt", 0.9, 3),
        hit(
            "definition",
            4,
            "src/lib.rs",
            0.8,
            4,
            Some(code(SymbolType::Function, "loadWidget", None)),
        ),
    ];
    let out = run("loadWidget", candidates.clone(), &QueryFilter::default(), 3);
    assert_eq!(
        out.iter()
            .map(|hit| (hit.entity.id().as_str(), hit.rank))
            .collect::<Vec<_>>(),
        [("guide", 1), ("definition", 2), ("notes", 3)]
    );
    for index in [0, 2] {
        assert_eq!(out[index].score, candidates[index].score);
        assert!(out[index].trace.as_ref().expect("trace").ranking.is_none());
    }
    let trace = out[1].trace.as_ref().expect("trace");
    assert_eq!((trace.fusion.rank, trace.fusion.score), (4, 0.8));
    assert_eq!(trace.ranking.as_ref().expect("code ranking").rank, 2);
    assert!(trace.final_selection.returned_by_limit);
    assert_eq!(trace.final_selection.cutoff_rank, 3);

    let out = run("loadWidget", candidates, &QueryFilter::default(), 1);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].entity.id().as_str(), "guide");
}

#[test]
fn documents_do_not_affect_code_dependency_or_diversity_factors() {
    let candidates = vec![
        hit("first", 1, "vendor/lib.rs", 1.0, 1, None),
        document("guide", 2, "docs/guide.md", 0.95, 2),
        hit("second", 1, "vendor/lib.rs", 0.9, 3, None),
    ];
    let out = run("behavior", candidates, &QueryFilter::default(), 5);
    assert_eq!(out.len(), 3);
    let trace = out[2].trace.as_ref().expect("trace");
    let ranking = trace.ranking.as_ref().expect("code ranking");
    assert_eq!(out[2].entity.id().as_str(), "second");
    assert_eq!(ranking.path_factor, 1.0);
    assert_eq!(ranking.diversity_factor, 1.0);
    assert_eq!(ranking.rank, 3);
    assert_eq!(trace.final_selection.cutoff_rank, 5);
}

#[test]
fn exact_names_and_qualified_scopes_outweigh_references() {
    let out = run(
        "Vitest",
        vec![
            hit(
                "reference",
                1,
                "plugin.ts",
                1.0,
                1,
                Some(code(SymbolType::Value, "vitest", None)),
            ),
            hit(
                "definition",
                2,
                "core.ts",
                0.9,
                2,
                Some(code(SymbolType::Class, "Vitest", None)),
            ),
        ],
        &QueryFilter::default(),
        1,
    );
    assert_eq!(out[0].entity.id().as_str(), "definition");
    let trace = out[0].trace.as_ref().expect("trace");
    assert_eq!(trace.fusion.rank, 2);
    assert_eq!(trace.ranking.as_ref().expect("ranking").rank, 1);
    assert!(trace.final_selection.returned_by_limit);
    let features = QueryFeatures::new("Router.merge");
    assert_eq!(
        features
            .symbol_bonus(&code(SymbolType::Function, "merge", Some("Router")))
            .0,
        "qualified_exact"
    );
    assert_ne!(
        features
            .symbol_bonus(&code(SymbolType::Function, "merge", Some("Other")))
            .0,
        "qualified_exact"
    );
}

#[test]
fn constants_aliases_and_unicode_definitions_are_not_short_fragment_noise() {
    for (query, kind) in [
        ("MAX_CONNECTIONS", SymbolType::Value),
        ("UserId", SymbolType::Alias),
        ("查找用户", SymbolType::Function),
    ] {
        let out = run(
            query,
            vec![
                hit(
                    "unrelated",
                    1,
                    "a.rs",
                    1.0,
                    1,
                    Some(code(SymbolType::Class, "Other", None)),
                ),
                hit("target", 2, "b.rs", 0.9, 2, Some(code(kind, query, None))),
            ],
            &QueryFilter::default(),
            1,
        );
        assert_eq!(out[0].entity.id().as_str(), "target", "{query}");
    }
    assert!(
        QueryFeatures::new("constant MAX_CONNECTIONS")
            .symbol_bonus(&code(SymbolType::Value, "MAX_CONNECTIONS", None))
            .2
            > 0.0
    );
}

#[test]
fn usage_queries_and_explicit_types_control_definition_bonus() {
    let class = code(SymbolType::Class, "Reporter", None);
    let interface = code(SymbolType::Interface, "Reporter", None);
    let features = QueryFeatures::new("interface Reporter");
    assert_eq!(features.symbol_bonus(&class).2, 0.0);
    assert!(features.symbol_bonus(&interface).2 > 0.0);
    assert_eq!(
        QueryFeatures::new("callers of Reporter")
            .symbol_bonus(&class)
            .2,
        0.0
    );
    assert_eq!(
        QueryFeatures::new("谁调用 Reporter").symbol_bonus(&class).2,
        0.0
    );
    assert_eq!(
        QueryFeatures::new("get")
            .symbol_bonus(&code(SymbolType::Function, "get", None))
            .1,
        0.0
    );
    assert_eq!(
        QueryFeatures::new("standard type Reporter implementations")
            .symbol_bonus(&interface)
            .2,
        0.0
    );
}

#[test]
fn name_tokens_split_camel_case_without_counting_query_repetition() {
    let metadata = code(SymbolType::Function, "createHTTPServer", None);
    let a = QueryFeatures::new("create http server").symbol_bonus(&metadata);
    let b = QueryFeatures::new("server create create http").symbol_bonus(&metadata);
    assert_eq!(a, b);
    assert_eq!(a.0, "tokens");
    assert!(a.1 > 0.0);
}

#[test]
fn paths_use_components_and_do_not_stack_penalties() {
    let none = Roles::default();
    for path in [
        "contest/latest.ts",
        "src/vendor_service.rs",
        "src/testing/runner.ts",
        "dist/index.ts",
        "types/index.d.ts",
    ] {
        assert_eq!(Roles::from_path(path).factor(none), 1.0, "{path}");
    }
    assert_eq!(
        Roles::from_path("vendor/tests/fixtures/foo.test.ts").factor(none),
        0.60
    );
    assert_eq!(Roles::from_path(r"src\__tests__\foo.ts").factor(none), 0.65);
    assert_eq!(
        Roles::from_path("THIRD_PARTY/lib/index.ts").factor(none),
        0.60
    );
    assert_eq!(
        Roles::from_path("types/index.generated.ts").factor(none),
        0.55
    );
}

#[test]
fn explicit_test_intent_and_narrow_filters_restore_path_prior() {
    let roles = Roles::from_path("tests/foo.test.ts");
    let features = QueryFeatures::new("unit tests for foo");
    assert_eq!(
        roles.factor(Roles::overrides(&features, &QueryFilter::default())),
        1.0
    );
    assert!(!QueryFeatures::new("test reporter interface").tests);
    assert!(!QueryFeatures::new("dependency injection container").dependencies);
    for query in ["tests/foo.ts", "foo.test.ts", r"tests\foo.ts"] {
        assert_eq!(
            roles.factor(Roles::overrides(
                &QueryFeatures::new(query),
                &QueryFilter::default()
            )),
            1.0
        );
    }

    let mut filter = QueryFilter {
        globs: vec!["tests/**".into()],
        ..QueryFilter::default()
    };
    assert_eq!(
        roles.factor(Roles::overrides(&QueryFeatures::default(), &filter)),
        1.0
    );
    filter.globs.push("src/**".into());
    assert_eq!(
        roles.factor(Roles::overrides(&QueryFeatures::default(), &filter)),
        0.65
    );
    filter.globs = vec!["!src/**".into()];
    assert_eq!(
        roles.factor(Roles::overrides(&QueryFeatures::default(), &filter)),
        0.65
    );
}

#[test]
fn dependency_only_scope_has_no_common_penalty() {
    let out = run(
        "behavior",
        vec![hit("a", 1, "vendor/lib.rs", 0.8, 1, None)],
        &QueryFilter::default(),
        1,
    );
    assert_eq!(out[0].score, 0.8);
    let out = run(
        "vendor behavior",
        vec![
            hit("a", 1, "vendor/lib.rs", 0.8, 1, None),
            hit("b", 2, "src/lib.rs", 0.7, 2, None),
        ],
        &QueryFilter::default(),
        1,
    );
    assert_eq!(out[0].entity.id().as_str(), "a");
}

#[test]
fn diversity_uses_selected_files_and_keeps_complementary_results() {
    let candidates = vec![
        hit("a", 1, "src/a.rs", 1.0, 1, None),
        hit("b", 1, "src/a.rs", 0.95, 2, None),
        hit("c", 2, "src/b.rs", 0.8, 3, None),
    ];
    let out = run("behavior", candidates.clone(), &QueryFilter::default(), 3);
    assert_eq!(
        out.iter()
            .map(|h| h.entity.id().as_str())
            .collect::<Vec<_>>(),
        ["a", "c", "b"]
    );
    let second = out[1]
        .trace
        .as_ref()
        .expect("trace")
        .ranking
        .as_ref()
        .expect("ranking");
    assert_eq!(second.file_count_at_selection, 0);
    let out = run(
        "behavior",
        candidates[..2].to_vec(),
        &QueryFilter::default(),
        2,
    );
    assert_eq!(out[1].score, 0.95);
}

#[test]
fn ties_and_missing_metadata_are_stable_and_trace_is_optional() {
    let mut candidates = vec![
        hit("b", 1, "a.rs", 0.8, 1, None),
        hit("a", 2, "b.rs", 0.8, 1, None),
    ];
    for h in &mut candidates {
        h.trace = None;
    }
    for _ in 0..2 {
        let out = run("behavior", candidates.clone(), &QueryFilter::default(), 2);
        assert_eq!(out[0].entity.id().as_str(), "a");
        assert!(out.iter().all(|h| h.trace.is_none()));
        candidates.reverse();
    }
    assert!(run("behavior", candidates, &QueryFilter::default(), 0).is_empty());
}

#[test]
fn natural_language_paths_outweigh_an_incidental_property() {
    let out = run(
        "watcher for file changes in watch mode",
        vec![
            hit(
                "property",
                1,
                "src/config.ts",
                0.78,
                1,
                Some(code(SymbolType::Value, "watch", None)),
            ),
            hit(
                "implementation",
                2,
                "src/watcher.ts",
                0.80,
                2,
                Some(code(SymbolType::Function, "processChanges", None)),
            ),
        ],
        &QueryFilter::default(),
        1,
    );
    assert_eq!(out[0].entity.id().as_str(), "implementation");
}

#[test]
fn weak_property_match_does_not_beat_a_more_specific_function() {
    let out = run(
        "how watch handles scheduled execution",
        vec![
            hit(
                "property",
                1,
                "src/a.ts",
                0.80,
                1,
                Some(code(SymbolType::Value, "watch", None)),
            ),
            hit(
                "implementation",
                2,
                "src/b.ts",
                0.80,
                2,
                Some(code(SymbolType::Function, "handleWatch", None)),
            ),
        ],
        &QueryFilter::default(),
        1,
    );
    assert_eq!(out[0].entity.id().as_str(), "implementation");
}

#[test]
fn pure_symbol_definition_can_recover_from_a_low_fusion_rank() {
    let out = run(
        "WidgetManager",
        vec![
            hit(
                "reference",
                1,
                "src/plugin.ts",
                1.0,
                1,
                Some(code(SymbolType::Value, "widgetManager", None)),
            ),
            hit(
                "definition",
                2,
                "src/core.ts",
                0.45,
                30,
                Some(code(SymbolType::Class, "WidgetManager", None)),
            ),
        ],
        &QueryFilter::default(),
        1,
    );
    assert_eq!(out[0].entity.id().as_str(), "definition");
}

#[test]
fn path_affinity_respects_components_and_portable_separators() {
    let query = QueryFeatures::new("HTTP request handler");
    assert!(
        query.path_affinity("adapters/node-http/requestHandler.ts")
            > query.path_affinity("src/config.ts")
    );
    assert_eq!(
        query.path_affinity("adapters/node-http/requestHandler.ts"),
        query.path_affinity(r"adapters\node-http\requestHandler.ts")
    );
    assert_eq!(
        QueryFeatures::new("src index main").path_affinity("src/index.ts"),
        0.0
    );
    assert_eq!(
        QueryFeatures::new("watcher.ts").path_affinity("src/watcher.ts"),
        1.0
    );
    assert_ne!(
        QueryFeatures::new("watcher.ts").path_affinity("src/otherwatcher.ts"),
        1.0
    );
}

#[test]
fn function_anchors_match_complete_paths_stems_and_scope_components() {
    let query = QueryFeatures::new("How does sentPacketHistory use Router.Dial?");
    assert!(query.names_function_file_or_scope(r"src\sent_packet_history.go", None));
    assert!(query.names_function_file_or_scope("http.go", Some("pkg::client.Router")));
    assert!(!query.names_function_file_or_scope("sent_packet_history_test.go", Some("MyRouter")));
    assert!(!query.names_function_file_or_scope("src/how.go", Some("router")));
    assert!(
        !QueryFeatures::new("how does packet history work")
            .names_function_file_or_scope("src/history.go", Some("packet"))
    );
    assert!(QueryFeatures::new("Semaphore").names_function_file_or_scope("semaphore.rs", None));
    let path = QueryFeatures::new(r"Read the functions in src\dispatch.ts?");
    assert!(path.names_function_file_or_scope("project/src/dispatch.ts", None));
    assert!(!path.names_function_file_or_scope("project/other/dispatch.ts", None));
    assert!(
        QueryFeatures::new("methods of `worker`")
            .names_function_file_or_scope("src/worker.c", None)
    );
}

#[test]
fn explicit_paths_do_not_contribute_standalone_names() {
    for literal in ["src/admin/user_service.py", r"src\admin\user_service.py"] {
        let query = QueryFeatures::new(&format!("inspect `{literal}` with OtherService"));
        assert!(query.names_function_file_or_scope("project/src/admin/user_service.py", None));
        assert!(!query.names_function_file_or_scope("src/public/user_service.py", None));
        assert!(!query.names_function_file_or_scope("other.py", Some("user_service")));
        assert!(query.names_function_file_or_scope("src/other_service.rs", None));
    }
}

#[test]
fn comparisons_preserve_both_literal_paths() {
    let query = QueryFeatures::new("Compare x<limit in src/lower.rs with x>limit in src/upper.rs");
    for path in ["src/lower.rs", "src/upper.rs"] {
        assert!(query.names_function_file_or_scope(path, None));
        assert_eq!(query.path_affinity(path), 1.0);
    }
    assert!(!query.names_function_file_or_scope("other/lower.rs", None));
}

#[test]
fn source_filenames_anchor_only_the_named_file() {
    for name in ["Router.cpp", "Router.test.cpp", "User-Router.cpp"] {
        let query = QueryFeatures::new(&format!("list methods in {name}"));
        assert!(query.names_function_file_or_scope(&format!("src/{name}"), None));
        assert!(!query.names_function_file_or_scope("include/Router.h", Some("net::Router")));
        assert!(!query.names_function_file_or_scope("include/User.h", Some("User")));
    }
    assert!(
        QueryFeatures::new("Router.Dial")
            .names_function_file_or_scope("dispatch.go", Some("net::Router"))
    );
    let call = QueryFeatures::new("Router.patch()");
    assert!(call.names_function_file_or_scope("dispatch.rs", Some("net::Router")));
    assert!(!call.names_function_file_or_scope("dispatch.rs", Some("OtherRouter")));
    let file = QueryFeatures::new("Router.patch");
    assert!(file.names_function_file_or_scope("changes/Router.patch", None));
    assert!(!file.names_function_file_or_scope("dispatch.rs", Some("net::Router")));
}

#[test]
fn generic_queries_anchor_the_container_only() {
    for query in [
        "Vec<String>",
        "Vec::<String>",
        "inspect `Vec<Result<String, Error>>` methods",
    ] {
        let query = QueryFeatures::new(query);
        assert!(query.names_function_file_or_scope("collections.rs", Some("alloc::Vec<T>")));
        assert!(query.names_function_file_or_scope("vec.rs", None));
        assert!(!query.names_function_file_or_scope("string.rs", None));
        assert!(!query.names_function_file_or_scope("other.rs", Some("String")));
        assert!(!query.names_function_file_or_scope("result.rs", Some("Error")));
    }
    let query = QueryFeatures::new("alloc::Vec<String> and OtherType");
    assert!(query.names_function_file_or_scope("collections.rs", Some("alloc::Vec<T>")));
    assert!(!query.names_function_file_or_scope("vec.rs", Some("legacy::Vec<T>")));
    assert!(query.names_function_file_or_scope("other_type.rs", None));
}

#[test]
fn qualified_scopes_require_the_whole_component_suffix() {
    let query = QueryFeatures::new("methods of admin::Router");
    for scope in ["admin::Router", "pkg::admin.Router"] {
        assert!(query.names_function_file_or_scope("dispatch.rs", Some(scope)));
    }
    for scope in ["legacy::Router", "notadmin::Router", "Router"] {
        assert!(!query.names_function_file_or_scope("router.rs", Some(scope)));
    }
}

#[test]
fn rust_generic_scopes_match_the_type_identifier() {
    let parser = QueryFeatures::new("list methods of Parser");
    assert!(parser.names_function_file_or_scope("jsonc.rs", Some("Parser<'a>")));
    assert!(!parser.names_function_file_or_scope("jsonc.rs", Some("OtherParser<'a>")));
    assert!(!parser.names_function_file_or_scope("jsonc.rs", Some("Wrapper<ast::Parser>")));

    let qualified = QueryFeatures::new("methods of jsonc::Parser");
    let scope = "crate::jsonc::Parser<'a, Vec<ast::Node>>";
    assert!(qualified.names_function_file_or_scope("dispatch.rs", Some(scope)));
    assert!(!qualified.names_function_file_or_scope("dispatch.rs", Some("legacy::Parser<'a>")));
    let scope = "Registry<fn() -> ast::Node>::jsonc::Parser<'a>";
    assert!(qualified.names_function_file_or_scope("dispatch.rs", Some(scope)));
}

#[test]
fn leading_qualified_types_anchor_their_methods() {
    for query in [
        "Router::{connect, disconnect}",
        "Router.connect",
        "Router::connect",
    ] {
        let query = QueryFeatures::new(query);
        assert!(query.names_function_file_or_scope("dispatch.rs", Some("net::Router")));
    }
}

fn with_range(mut hit: SearchHit, start: u64, end: u64) -> SearchHit {
    hit.entity = Entity::new(
        hit.entity.id().clone(),
        hit.entity.file_id(),
        Range::Byte(crate::domain::ByteRange::new(start, end).expect("byte range")),
        hit.entity.content().clone(),
        hit.entity.metadata().cloned(),
        hit.entity.fragments().to_vec(),
    )
    .expect("entity with range");
    hit
}

#[test]
fn java_method_waivers_respect_the_declared_package() {
    for (package, expected) in [
        (Some("example.java.util"), 1.0),
        (Some("other.java.util"), 1.0 / 1.5),
        (Some(""), 1.0 / 1.5),
        (None, 1.0 / 1.5),
    ] {
        let method = |name| {
            let mut metadata = code(SymbolType::Function, name, Some("List"));
            metadata.package = package.map(str::to_owned);
            Some(metadata)
        };
        let out = run(
            "methods of example.java.util.List",
            vec![
                with_range(hit("add", 1, "List.java", 1.0, 1, method("add")), 0, 10),
                with_range(hit("get", 1, "List.java", 0.9, 2, method("get")), 10, 20),
                hit("other", 2, "Other.java", 0.1, 3, None),
            ],
            &QueryFilter::default(),
            3,
        );
        let trace = out
            .iter()
            .find(|hit| hit.entity.id().as_str() == "get")
            .and_then(|hit| hit.trace.as_ref())
            .and_then(|trace| trace.ranking.as_ref())
            .expect("second method's ranking trace");
        assert_eq!(trace.file_count_at_selection, 1);
        assert_eq!(trace.diversity_factor, expected, "package {package:?}");
    }
}

#[test]
fn only_independent_anchored_functions_keep_their_scores() {
    let scoped = |kind, name| Some(code(kind, name, Some("net::Router")));
    let function = SymbolType::Function;
    let path = "src/dispatch.rs";
    let candidates = vec![
        with_range(
            hit("lead", 1, path, 1.0, 1, scoped(function, "first")),
            0,
            10,
        ),
        with_range(
            hit("sibling", 1, path, 0.9, 2, scoped(function, "next")),
            10,
            20,
        ),
        with_range(
            hit("overlap", 1, path, 0.85, 3, scoped(function, "nested")),
            12,
            18,
        ),
        with_range(
            hit("duplicate", 1, path, 0.84, 4, scoped(function, "first")),
            25,
            30,
        ),
        with_range(
            hit(
                "field",
                1,
                path,
                0.83,
                5,
                scoped(SymbolType::Value, "field"),
            ),
            31,
            32,
        ),
        hit(
            "unknown-range",
            1,
            path,
            0.82,
            6,
            scoped(function, "unknown"),
        ),
        with_range(hit("other-file", 2, "lib.rs", 0.8, 7, None), 0, 7),
    ];
    let out = run(
        "Router::{first, next}",
        candidates,
        &QueryFilter::default(),
        7,
    );
    let ids: Vec<_> = out.iter().map(|hit| hit.entity.id().as_str()).collect();
    assert_eq!(&ids[..3], ["lead", "sibling", "other-file"]);
    let trace = |index: usize| {
        out[index]
            .trace
            .as_ref()
            .expect("search trace")
            .ranking
            .as_ref()
            .expect("ranking trace")
    };
    assert_eq!(
        (trace(1).file_count_at_selection, trace(1).diversity_factor),
        (1, 1.0)
    );
    for (index, _) in out.iter().enumerate().skip(3) {
        let r = trace(index);
        assert!(r.file_count_at_selection >= 2);
        assert_eq!(
            r.diversity_factor,
            1.0 / (1.0 + 0.5 * f64::from(r.file_count_at_selection))
        );
    }
}

#[test]
fn unknown_selected_range_prevents_later_waivers() {
    let named = Some(code(SymbolType::Function, "send", None));
    let out = run(
        "inspect `dispatch.rs`",
        vec![
            hit("whole", 1, "dispatch.rs", 1.0, 1, None),
            with_range(hit("function", 1, "dispatch.rs", 0.8, 2, named), 2, 6),
            hit("other", 2, "another.rs", 0.1, 3, None),
        ],
        &QueryFilter::default(),
        3,
    );
    let r = out
        .iter()
        .find(|h| h.entity.id().as_str() == "function")
        .expect("returned function")
        .trace
        .as_ref()
        .expect("search trace")
        .ranking
        .as_ref()
        .expect("ranking trace");
    assert_eq!(
        (r.file_count_at_selection, r.diversity_factor),
        (1, 1.0 / 1.5)
    );
}

#[test]
fn file_support_requires_independent_definitions_and_is_bounded() {
    let mut candidates = vec![
        hit(
            "property",
            1,
            "a.ts",
            0.8,
            1,
            Some(code(SymbolType::Value, "parse", None)),
        ),
        hit(
            "definition",
            2,
            "b.ts",
            0.75,
            2,
            Some(code(SymbolType::Function, "parseRequest", None)),
        ),
        hit(
            "helper",
            2,
            "b.ts",
            0.7,
            3,
            Some(code(SymbolType::Function, "readBody", None)),
        ),
    ];
    for (index, candidate) in candidates.iter_mut().enumerate() {
        let offset = u64::try_from(index * 100).expect("offset");
        candidate.entity = Entity::new(
            candidate.entity.id().clone(),
            candidate.entity.file_id(),
            Range::Byte(crate::domain::ByteRange::new(offset, offset + 50).expect("source range")),
            candidate.entity.content().clone(),
            candidate.entity.metadata().cloned(),
            candidate.entity.fragments().to_vec(),
        )
        .expect("entity");
    }
    let routes = [ResolvedSearchRoute {
        id: "fts".into(),
        mode: ContextRouteMode::Fts,
        query: "parse request body".into(),
    }];
    let policy = Policy {
        support_weight: 0.15,
        ..Policy::BASELINE
    };
    let out = rank_with_policy(
        candidates.clone(),
        &routes,
        &QueryFilter::default(),
        1.0,
        3,
        policy,
        None,
    );
    assert_eq!(out[0].entity.id().as_str(), "definition");
    let ranking = out[0]
        .trace
        .as_ref()
        .expect("trace")
        .ranking
        .as_ref()
        .expect("ranking");
    assert_eq!(ranking.file_support_count, 1);
    assert!(ranking.file_support_bonus > 0.0 && ranking.file_support_bonus <= 0.15);
    candidates[2].entity = Entity::new(
        candidates[2].entity.id().clone(),
        candidates[2].entity.file_id(),
        candidates[1].entity.source_range(),
        candidates[2].entity.content().clone(),
        candidates[2].entity.metadata().cloned(),
        candidates[2].entity.fragments().to_vec(),
    )
    .expect("entity");
    let out = rank_with_policy(
        candidates,
        &routes,
        &QueryFilter::default(),
        1.0,
        3,
        policy,
        None,
    );
    assert!(out.iter().all(|hit| {
        hit.trace
            .as_ref()
            .expect("trace")
            .ranking
            .as_ref()
            .expect("ranking")
            .file_support_bonus
            == 0.0
    }));
}

#[test]
fn ranking_traces_default_omitted_evidence_fields() {
    let out = run(
        "Widget",
        vec![hit(
            "a",
            1,
            "a.ts",
            1.0,
            1,
            Some(code(SymbolType::Class, "Widget", None)),
        )],
        &QueryFilter::default(),
        1,
    );
    let mut value =
        serde_json::to_value(out[0].trace.as_ref().expect("trace")).expect("trace JSON");
    let ranking = value["ranking"].as_object_mut().expect("ranking object");
    for field in ["path_bonus", "file_support_bonus", "file_support_count"] {
        ranking.remove(field);
    }
    let trace: SearchHitTrace = serde_json::from_value(value).expect("trace with omitted evidence");
    let ranking = trace.ranking.expect("ranking");
    assert_eq!(ranking.path_bonus, 0.0);
    assert_eq!(ranking.file_support_bonus, 0.0);
    assert_eq!(ranking.file_support_count, 0);
}
