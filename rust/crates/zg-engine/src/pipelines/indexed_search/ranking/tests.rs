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
        is_supplemental: false,
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
        untraced_recall: Vec::new(),
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

fn supplemental_recall_pool() -> Vec<SearchHit> {
    use super::super::pipeline::{SearchEvidence, reciprocal_rank};
    use crate::api::context::result::SearchRecallTrace;
    fn routes(mut hit: SearchHit, entries: &[(&str, usize)]) -> SearchHit {
        let trace = hit.trace.as_mut().expect("raw trace");
        for &(id, rank) in entries {
            trace.recall.push(SearchRecallTrace {
                path: if id == "vector" {
                    ContextRouteMode::Vector
                } else {
                    ContextRouteMode::Fts
                },
                route_id: id.into(),
                query: "query".into(),
                found: true,
                rank: Some(rank),
                score: Some(1.0),
                forced: false,
                reason: None,
            });
        }
        hit.score = entries.iter().map(|(_, rank)| reciprocal_rank(*rank)).sum();
        trace.fusion.score = hit.score;
        hit
    }
    let marker = CodeMetadata {
        is_supplemental: true,
        ..CodeMetadata::default()
    };
    let mut first = routes(
        hit("statement-1", 1, "a.rs", 0.0, 3, Some(marker.clone())),
        &[("fts", 1), ("vector", 7)],
    );
    // Multiple matching fragments from this statement still supply one FTS rank.
    let evidence = SearchEvidence {
        fragment: first.entity.fragments()[0].clone(),
    };
    first.evidence = vec![evidence.clone(), evidence];
    let second = routes(
        hit("statement-2", 1, "a.rs", 0.0, 4, Some(marker)),
        &[("fts", 3)],
    );
    let mut named = routes(
        fixture_hit(
            "symbol",
            1,
            "a.rs",
            0.0,
            2,
            Content::Code("lookup(name, id)".into()),
            Some(code(SymbolType::Function, "dispatch", None)),
        ),
        &[("fts", 4), ("fts-2", 2), ("vector", 8)],
    );
    named.evidence.push(SearchEvidence {
        fragment: named.entity.fragments()[0].clone(),
    });
    vec![
        document("guide", 2, "guide.md", 1.0, 1),
        named,
        first,
        second,
    ]
}

#[test]
fn general_queries_adjust_independent_route_ranks_once_per_supplemental_entity() {
    use super::super::pipeline::reciprocal_rank;
    let pool = supplemental_recall_pool();
    let original = pool[1].score;
    for (query, changes) in [
        ("How does this work?", true),
        ("KEY_NAME", false),
        ("Where is KEY_NAME defined?", false),
        ("\"some phrase\"", false),
        ("lookup(name, id)", false),
        ("甲与乙如何协作", false),
    ] {
        let result = run(query, pool.clone(), &QueryFilter::default(), 4);
        assert_eq!(result[0].entity.id().as_str(), "guide");
        let hit = result
            .iter()
            .find(|h| h.entity.id().as_str() == "symbol")
            .expect("regular code");
        let raw = hit.trace.as_ref().expect("raw trace");
        assert_eq!(raw.fusion.score, original);
        assert_eq!(raw.fusion.rank, 2);
        assert_eq!(raw.recall[0].rank, Some(4));
        let ranked = raw.ranking.as_ref().expect("ranking");
        assert_eq!(ranked.normalized_base, original);
        if changes {
            assert_eq!(
                ranked.adjusted_route_ranks,
                BTreeMap::from([("fts".into(), 2), ("vector".into(), 7)])
            );
            assert_eq!(
                ranked.static_score,
                reciprocal_rank(2) + reciprocal_rank(2) + reciprocal_rank(7)
            );
        } else {
            assert!(ranked.adjusted_route_ranks.is_empty(), "{query}");
        }
        let untraced = pool
            .iter()
            .cloned()
            .map(|mut item| {
                item.untraced_recall = item.trace.take().expect("fixture trace").recall;
                item
            })
            .collect();
        let without = run(query, untraced, &QueryFilter::default(), 4);
        assert!(
            result
                .iter()
                .zip(&without)
                .all(|(a, b)| a.entity.id() == b.entity.id() && a.score == b.score)
        );
    }
}

#[test]
fn a_complete_test_title_waives_only_the_matching_test_path_penalty() {
    let mut target = fixture_hit(
        "callback",
        1,
        "tests/selection.ts",
        0.96,
        2,
        Content::Code("it('keeps all original ranks', () => {});".into()),
        None,
    );
    target
        .evidence
        .push(super::super::pipeline::SearchEvidence {
            fragment: target.entity.fragments()[0].clone(),
        });
    let competing = hit("function", 2, "src/selection.ts", 0.75, 1, None);
    for (q, target_wins) in [
        ("keeps all original ranks", true),
        ("\"keeps all original ranks\"", true),
        ("all original ranks", false),
        ("How does selection keep the original ranks?", false),
        ("ranks", false),
    ] {
        let out = run(
            q,
            vec![competing.clone(), target.clone()],
            &QueryFilter::default(),
            2,
        );
        let index = usize::from(!target_wins);
        let trace = out[index]
            .trace
            .as_ref()
            .expect("search trace")
            .ranking
            .as_ref()
            .expect("code ranking");
        assert_eq!(out[index].entity.id().as_str(), "callback", "{q}");
        assert_eq!(
            trace.path_overrides.contains(&"test".to_owned()),
            target_wins,
            "{q}"
        );
    }
}

#[test]
fn supplemental_statements_compete_normally_for_direct_lookups() {
    fn ranking(hit: &SearchHit) -> &SearchRankingTrace {
        hit.trace
            .as_ref()
            .expect("trace")
            .ranking
            .as_ref()
            .expect("ranking")
    }
    let mut statement = fixture_hit(
        "statement",
        1,
        "a.rs",
        1.0,
        1,
        Content::Code("register(\"road cache\", loader, 加载器)".into()),
        Some(CodeMetadata {
            is_supplemental: true,
            signature: Some("缓存上限".into()),
            ..CodeMetadata::default()
        }),
    );
    statement
        .evidence
        .push(super::super::pipeline::SearchEvidence {
            fragment: statement.entity.fragments()[0].clone(),
        });
    let legacy = hit("fallback", 1, "a.rs", 0.9, 2, None);
    let named = hit(
        "function",
        1,
        "a.rs",
        0.8,
        3,
        Some(code(SymbolType::Function, "dispatch", None)),
    );
    for (query, expected_factor) in [
        ("How does `resolve` call `dispatch`?", 0.5),
        ("`resolve` and `dispatch`", 0.5),
        ("How does the field CACHE_KEY affect lookup?", 0.5),
        ("What is CACHE_KEY and CACHE_SIZE set to?", 0.5),
        ("foo和bar如何协作", 0.5),
        ("加载", 0.5),
        ("缓存上限", 1.0),
        ("加载器", 1.0),
        ("`缓存上限`", 1.0),
        ("`未知名称`", 0.5),
        ("What is 缓存上限 set to?", 1.0),
        ("缓存上限的值是多少？", 1.0),
        ("其他设置的值是多少？", 0.5),
        ("DEFAULT_LIMIT", 1.0),
        ("loader", 1.0),
        ("module::CACHE_KEY", 1.0),
        ("What is the value of `CACHE_KEY`?", 1.0),
        ("What is `CACHE_KEY` set to?", 1.0),
        ("field cache_size", 1.0),
        ("\"road cache\"", 1.0),
        ("-42", 1.0),
        ("register(\"road cache\", loader, 加载器)", 1.0),
    ] {
        let out = run(
            query,
            vec![statement.clone(), legacy.clone(), named.clone()],
            &QueryFilter::default(),
            3,
        );
        let target = out
            .iter()
            .find(|hit| hit.entity.id() == statement.entity.id())
            .expect("supplemental statement");
        let trace = ranking(target);
        assert_eq!(trace.supplemental_factor, Some(expected_factor), "{query}");
        assert_eq!(
            target.score,
            (1.0 + (trace.symbol_bonus + trace.definition_bonus)) * expected_factor,
            "{query}"
        );
        assert_eq!(target.rank == 1, expected_factor == 1.0, "{query}");
        let old = out
            .iter()
            .find(|hit| hit.entity.id() == legacy.entity.id())
            .expect("existing fallback");
        assert_eq!(old.score, 0.9);
        assert_eq!(ranking(old).supplemental_factor, None);
    }
    let out = run(
        "How does the cache work?",
        vec![statement, legacy],
        &QueryFilter::default(),
        2,
    );
    assert_eq!(
        out[0].score, 1.0,
        "a pool with no symbols has no competition prior"
    );
    assert_eq!(ranking(&out[0]).supplemental_factor, Some(1.0));
}

#[test]
fn exact_bound_declarations_can_overtake_source_mentions_at_the_cutoff() {
    let binding = CodeMetadata {
        is_supplemental: true,
        signature: Some("ROUTE_DEFAULTS".into()),
        ..CodeMetadata::default()
    };
    let mut pool: Vec<_> = (1..=10)
        .map(|rank| {
            fixture_hit(
                &rank.to_string(),
                1,
                "a.rs",
                1.0,
                rank,
                Content::Code("read(ROUTE_DEFAULTS)".into()),
                Some(code(SymbolType::Function, "load", None)),
            )
        })
        .collect();
    pool.push(hit("table", 1, "a.rs", 0.15, 11, Some(binding.clone())));
    let out = run("ROUTE_DEFAULTS", pool, &QueryFilter::default(), 10);
    assert_eq!(out[0].entity.id().as_str(), "table");
    let trace = out[0].trace.as_ref().expect("trace");
    assert_eq!(trace.fusion.rank, 11);
    let ranking = trace.ranking.as_ref().expect("ranking");
    assert_eq!(ranking.symbol_match, "binding_exact");
    assert_eq!(
        (ranking.symbol_bonus, ranking.definition_bonus),
        (0.25, 0.85)
    );

    for query in [
        "What is `ROUTE_DEFAULTS` set to?",
        "Where is ROUTE_DEFAULTS defined?",
    ] {
        assert_eq!(
            QueryFeatures::new(query).symbol_bonus(&binding),
            ("binding_exact", 0.25, 0.40)
        );
    }
    for query in [
        "route_defaults",
        "ROUTE_DEFAULT",
        "\"ROUTE_DEFAULTS\"",
        "How does ROUTE_DEFAULTS affect startup?",
        "Where are ROUTE_DEFAULTS and ROUTES defined?",
    ] {
        assert_eq!(
            QueryFeatures::new(query).symbol_bonus(&binding),
            ("none", 0.0, 0.0),
            "{query}"
        );
    }
    let unmarked = CodeMetadata {
        is_supplemental: false,
        ..binding
    };
    assert_eq!(
        QueryFeatures::new("ROUTE_DEFAULTS").symbol_bonus(&unmarked),
        ("none", 0.0, 0.0)
    );
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
    assert_eq!(ranking.supplemental_factor, None);
    assert_eq!(ranking.file_support_bonus, 0.0);
    assert_eq!(ranking.file_support_count, 0);
}
