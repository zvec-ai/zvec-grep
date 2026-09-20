//! Direct file search, request normalization, and context-result assembly.

mod structure;

use std::{path::Path, time::Instant};

use crate::{
    EngineError, EngineResult,
    api::context::{
        ContextOptions, ContextResult,
        result::{
            ContextContainer, ContextContentRole, ContextCoverage, ContextDiagnostics, ContextItem,
            ContextItemKind, ContextItemStatus, ContextSource, EmptyReason, MatchedBy,
            RgDiagnostics, TimingEntry,
        },
    },
    lexical::{
        LexicalSearchService,
        types::{LexicalCoverage, LexicalOptions, LexicalSearchReply, LexicalSearchRequest},
    },
};

use self::structure::{
    EnrichedLexicalMatch, StructureEnrichmentResult, enrich_lexical_matches_with_structure,
};

const DEFAULT_MAX_CONCURRENT_SEARCHES: usize = 2;

#[derive(Clone, Debug)]
pub(crate) struct DirectSearchService {
    lexical: LexicalSearchService,
}

impl DirectSearchService {
    pub(crate) fn new() -> Self {
        Self {
            lexical: LexicalSearchService::default()
                .with_max_searches(DEFAULT_MAX_CONCURRENT_SEARCHES),
        }
    }

    pub(crate) async fn context(&self, options: ContextOptions) -> EngineResult<ContextResult> {
        let root = std::path::absolute(options.root.as_deref().unwrap_or_else(|| Path::new(".")))
            .map_err(|error| {
            EngineError::from_io("failed to resolve workspace root", &error)
        })?;
        let normalized = normalize_request(&options)?;
        if !options.rg_options.extra_args.is_empty() {
            return Err(EngineError::unsupported(
                "context rg_options.extra_args is not supported by the embedded backend",
            ));
        }
        let structure_max_file_size_bytes = options.max_file_size_bytes;
        let modified_after_epoch_ms = options.rg_options.modified_after_epoch_ms;
        let modified_before_epoch_ms = options.rg_options.modified_before_epoch_ms;
        let request = LexicalSearchRequest {
            root: Some(root.clone()),
            patterns: normalized.patterns,
            pattern_files: options.rg_options.pattern_files.clone(),
            paths: options.rg_paths,
            limit: options.limit,
            options: LexicalOptions {
                matching: options.rg_options,
                hidden: options.hidden,
                no_ignore: options.no_ignore,
                follow: options.follow,
                globs: options.globs,
                insensitive_globs: options.insensitive_globs,
                file_types: options.file_types,
                excluded_file_types: options.excluded_file_types,
                ignore_files: options.ignore_files,
                max_depth: options.max_depth,
                max_file_size_bytes: options.max_file_size_bytes,
                modified_after_epoch_ms,
                modified_before_epoch_ms,
            },
        };
        let mut reply = self.lexical.search(&root, &request).await?;
        let structure_started = Instant::now();
        let enrichment = enrich_lexical_matches_with_structure(
            &root,
            std::mem::take(&mut reply.matches),
            structure_max_file_size_bytes,
        );
        let structure_duration = structure_started.elapsed();
        let mut result = context_from_lexical(normalized.display_query, reply, enrichment);
        result.diagnostics.timings.push(TimingEntry {
            name: "structure_enrichment".to_owned(),
            duration_micros: structure_duration
                .as_micros()
                .try_into()
                .unwrap_or(u64::MAX),
            count: None,
        });
        Ok(result)
    }
}

impl Default for DirectSearchService {
    fn default() -> Self {
        Self::new()
    }
}

struct NormalizedRequest {
    display_query: String,
    patterns: Vec<String>,
}

fn normalize_request(options: &ContextOptions) -> EngineResult<NormalizedRequest> {
    // Empty patterns and surrounding whitespace are meaningful to ripgrep.
    let primary_patterns = options
        .query
        .iter()
        .chain(&options.queries)
        .cloned()
        .collect::<Vec<_>>();
    let route_patterns = options
        .routes
        .iter()
        .enumerate()
        .map(|(index, route)| {
            let query = route.query.trim();
            if query.is_empty() {
                return Err(EngineError::invalid_argument(format!(
                    "context route {index} requires a non-empty query"
                )));
            }
            Ok(query.to_owned())
        })
        .collect::<EngineResult<Vec<_>>>()?;
    if primary_patterns.is_empty()
        && route_patterns.is_empty()
        && options.rg_options.pattern_files.is_empty()
    {
        return Err(EngineError::invalid_argument(
            "context requires a non-empty query or route",
        ));
    }
    let display_query = if !primary_patterns.is_empty() {
        primary_patterns.join(" | ")
    } else if !route_patterns.is_empty() {
        route_patterns.join(" | ")
    } else {
        options
            .rg_options
            .pattern_files
            .iter()
            .map(|path| format!("@{}", path.display()))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    Ok(NormalizedRequest {
        display_query,
        patterns: primary_patterns.into_iter().chain(route_patterns).collect(),
    })
}

fn context_from_lexical(
    query: String,
    reply: LexicalSearchReply,
    enrichment: StructureEnrichmentResult,
) -> ContextResult {
    let hits_returned = enrichment.items.len();
    let empty_reason = (hits_returned == 0).then_some({
        if !reply.diagnostics.missing_paths.is_empty()
            && reply.diagnostics.searched_paths.is_empty()
        {
            EmptyReason::NoSearchableFiles
        } else {
            EmptyReason::NoMatches
        }
    });
    ContextResult {
        freshness: None,
        background_refresh: None,
        query,
        root: reply.root,
        source: ContextSource::Rg,
        coverage: match reply.coverage {
            LexicalCoverage::Exhaustive => ContextCoverage::RgExhaustive,
            LexicalCoverage::Truncated => ContextCoverage::RgTruncated,
        },
        workspace_index: None,
        items: enrichment
            .items
            .into_iter()
            .map(
                |EnrichedLexicalMatch {
                     matched: item,
                     container,
                 }| ContextItem {
                    kind: ContextItemKind::LexicalMatch,
                    rank: item.rank,
                    absolute_path: item.absolute_path,
                    relative_path: item.relative_path,
                    range: item.range.into(),
                    excerpt_range: item.excerpt_range.map(Into::into),
                    content: item.content,
                    content_role: Some(ContextContentRole::Source),
                    status: ContextItemStatus::Fresh,
                    score: None,
                    matched_by: MatchedBy::Lexical,
                    metadata: container
                        .as_ref()
                        .and_then(|value| value.metadata.as_ref())
                        .cloned(),
                    entity_id: None,
                    container: container.map(|value| ContextContainer {
                        entity_id: None,
                        range: value.range.into(),
                        metadata: value.metadata,
                    }),
                    trace: None,
                    query_groups: Vec::new(),
                    selection_reason: None,
                    coverage_group: None,
                },
            )
            .collect(),
        group_results: Vec::new(),
        diagnostics: ContextDiagnostics {
            empty_reason,
            index: None,
            rg: Some(RgDiagnostics {
                backend: reply.diagnostics.backend,
                command: reply.diagnostics.command,
                args: reply.diagnostics.args,
                ignored_directories: reply.diagnostics.ignored_directories,
                missing_paths: reply.diagnostics.missing_paths,
                searched_paths: reply.diagnostics.searched_paths,
                limit: reply.diagnostics.limit,
                truncated: reply.diagnostics.truncated,
            }),
            structure: Some(enrichment.diagnostics),
            timings: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::api::context::options::{ContextRoute, ContextRouteMode, RgOptions};

    use super::{ContextOptions, normalize_request};

    #[tokio::test]
    async fn direct_search_returns_structure_without_index_identity() {
        use crate::api::context::result::{ContentRange, EntityMetadata};

        let directory = tempfile::tempdir().expect("source directory");
        let source = "pub fn orchard() {\n    let fruit = \"needle\";\n}\n";
        std::fs::write(directory.path().join("source.rs"), source).expect("source file");
        let engine = crate::ZvecGrep::new();
        let result = engine
            .context(ContextOptions {
                root: Some(directory.path().to_path_buf()),
                rg: true,
                query: Some("needle".into()),
                ..ContextOptions::default()
            })
            .await
            .expect("direct search without an index");

        assert!(result.workspace_index.is_none());
        assert_eq!(result.items.len(), 1);
        let item = &result.items[0];
        assert!(item.entity_id.is_none());
        assert!(item.content.contains("needle"));
        let container = item.container.as_ref().expect("function container");
        assert!(container.entity_id.is_none());
        assert!(matches!(
            &container.metadata,
            Some(EntityMetadata::Code(metadata))
                if metadata.symbol_name.as_deref() == Some("orchard")
        ));
        assert!(matches!(
            container.range,
            ContentRange::Text {
                start_line: 1,
                end_line: 3,
                start_byte_offset: 0,
                end_byte_offset,
                ..
            } if end_byte_offset == source.trim_end().len()
        ));
        let serialized = serde_json::to_value(container).expect("serialize container");
        assert_eq!(serialized.get("entity_id"), Some(&serde_json::Value::Null));
        assert!(!directory.path().join(".zvec-grep").exists());
        engine.close();
    }

    #[test]
    fn preserves_primary_patterns_and_normalizes_supplemental_routes() {
        let request = normalize_request(&ContextOptions {
            rg: true,
            query: Some(" alpha ".to_owned()),
            queries: vec![String::new(), " beta ".to_owned()],
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: " gamma ".to_owned(),
            }],
            ..ContextOptions::default()
        })
        .expect("normalized direct request");

        assert_eq!(request.display_query, " alpha  |  |  beta ");
        assert_eq!(request.patterns, [" alpha ", "", " beta ", "gamma"]);
    }

    #[test]
    fn accepts_pattern_files_without_inline_patterns() {
        let request = normalize_request(&ContextOptions {
            rg: true,
            rg_options: RgOptions {
                pattern_files: vec!["patterns.txt".into(), "more.txt".into()],
                ..RgOptions::default()
            },
            ..ContextOptions::default()
        })
        .expect("pattern files supply patterns");

        assert!(request.patterns.is_empty());
        assert_eq!(request.display_query, "@patterns.txt | @more.txt");
        assert!(
            normalize_request(&ContextOptions {
                rg: true,
                ..ContextOptions::default()
            })
            .is_err()
        );
    }

    #[test]
    fn accepts_routes_as_patterns_and_rejects_empty_routes() {
        let mut options = ContextOptions {
            rg: true,
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Vector,
                query: " needle ".to_owned(),
            }],
            ..ContextOptions::default()
        };
        let request = normalize_request(&options).expect("route supplies a pattern");
        assert_eq!(request.display_query, "needle");
        assert_eq!(request.patterns, ["needle"]);

        options.routes[0].query = " \n ".to_owned();
        assert!(normalize_request(&options).is_err());
    }
}
