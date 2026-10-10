// Neutral rules must preserve exact scores and configured factors.
#![allow(clippy::float_cmp)]

use super::*;
use crate::{
    api::context::{
        options::ContextRouteMode,
        result::{MatchedBy, SearchFinalTrace, SearchFusionTrace, SearchHitTrace},
    },
    domain::{
        CodeMetadata, Content, Entity, EntityId, FileIndexStatus, FileRecord, FileSnapshot, Range,
        SourcePath, SymbolType,
    },
};

fn code(kind: SymbolType, name: &str, scope: Option<&str>) -> CodeMetadata {
    CodeMetadata {
        symbol_type: Some(kind),
        symbol_name: Some(name.into()),
        scope: scope.map(str::to_owned),
        signature: None,
        documentation: None,
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
    SearchHit {
        entity: Entity {
            id: EntityId::from_string(id.to_owned()),
            file_id: FileId::new(file_id),
            source_range: Range::Full,
            content: Content::Text("source".into()),
            metadata: metadata.map(EntityMetadata::Code),
            fragments: vec![],
        },
        file: FileRecord {
            id: FileId::new(file_id),
            relative_path: SourcePath::new(path).expect("path"),
            snapshot: FileSnapshot {
                size_bytes: 6,
                modified_epoch_ms: Some(0),
                content_hash: None,
            },
            index_status: FileIndexStatus::Indexed {
                indexed_epoch_ms: 0,
                entity_count: 1,
            },
        },
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
    )
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
    assert_eq!(out[0].entity.id.as_str(), "definition");
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
        assert_eq!(out[0].entity.id.as_str(), "target", "{query}");
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
    assert_eq!(out[0].entity.id.as_str(), "a");
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
        out.iter().map(|h| h.entity.id.as_str()).collect::<Vec<_>>(),
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
        assert_eq!(out[0].entity.id.as_str(), "a");
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
    assert_eq!(out[0].entity.id.as_str(), "implementation");
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
    assert_eq!(out[0].entity.id.as_str(), "implementation");
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
    assert_eq!(out[0].entity.id.as_str(), "definition");
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
        candidate.entity.source_range =
            Range::Byte(crate::domain::ByteRange::new(offset, offset + 50).expect("source range"));
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
    );
    assert_eq!(out[0].entity.id.as_str(), "definition");
    let ranking = out[0]
        .trace
        .as_ref()
        .expect("trace")
        .ranking
        .as_ref()
        .expect("ranking");
    assert_eq!(ranking.file_support_count, 1);
    assert!(ranking.file_support_bonus > 0.0 && ranking.file_support_bonus <= 0.15);
    candidates[2].entity.source_range = candidates[1].entity.source_range;
    let out = rank_with_policy(candidates, &routes, &QueryFilter::default(), 1.0, 3, policy);
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
