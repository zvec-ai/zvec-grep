//! Search presentation fixtures; every visible byte comes from a production renderer.

use std::{
    cell::RefCell,
    io::{self, Write},
    path::{Path, PathBuf},
};

use zg_cli::{OutputOptions, PreviewMode};
use zg_engine::api::{
    context::{
        ContextResult,
        options::{ContextRouteMode, SymbolType},
        result::{
            CodeMetadata, ContentRange, ContextContentRole, ContextCoverage, ContextDiagnostics,
            ContextGroupResult, ContextItem, ContextItemKind, ContextItemStatus,
            ContextQueryGroupMatch, ContextQueryGroupRole, ContextSource, ContextWorkspaceIndex,
            EmptyReason, EntityMetadata, IndexDiagnostics, IndexRouteDiagnostics, MatchedBy,
            SearchFinalTrace, SearchFusionTrace, SearchHitTrace, SearchRecallTrace,
        },
    },
    paths::{PathLookupResult, PathsResult},
};

use super::{Fixture, options, render};

const SOURCE: &str = "pub fn refresh_index(root: &Path) -> Result<()> {\n    let files = scan_workspace(root)?;\n    let total = files.len();\n    let mut progress = IndexProgress::new(total);\n    for file in files {\n        index_file(&file)?;\n        progress.advance();\n    }\n    progress.optimizing();\n    optimize_index(root)?;\n    progress.saving();\n    save_manifest(root)?;\n    Ok(())\n}\n";

pub(super) fn fixtures(root: &Path) -> io::Result<Vec<Fixture>> {
    let mut fixtures = query_fixtures(root)?;
    fixtures.extend(preview_fixtures(root)?);
    fixtures.extend(lookup_fixtures(root)?);
    fixtures.extend(lexical_fixtures(root)?);
    Ok(fixtures)
}

fn source_range(content: &str) -> ContentRange {
    ContentRange::Text {
        start_line: 1,
        end_line: content.lines().count() + usize::from(content.ends_with('\n')),
        start_byte_offset: 0,
        end_byte_offset: content.len(),
        start_byte_column: 0,
        end_byte_column: if content.ends_with('\n') {
            0
        } else {
            content.lines().last().map_or(0, str::len)
        },
    }
}

fn indexed(root: &Path, matched_by: MatchedBy) -> ContextResult {
    let item = ContextItem {
        kind: ContextItemKind::IndexedEntity,
        rank: 1,
        absolute_path: root.join("src/index.rs"),
        relative_path: "src/index.rs".into(),
        range: source_range(SOURCE),
        excerpt_range: None,
        content_range: source_range(SOURCE),
        match_ranges: Vec::new(),
        content: SOURCE.into(),
        outline: None,
        content_role: Some(ContextContentRole::Source),
        status: ContextItemStatus::Fresh,
        score: Some(0.81),
        matched_by,
        metadata: Some(EntityMetadata::Code(CodeMetadata {
            symbol_type: Some(SymbolType::Function),
            symbol_name: Some("refresh_index".into()),
            scope: Some("indexing".into()),
            signature: Some("pub fn refresh_index(root: &Path) -> Result<()>".into()),
            documentation: None,
            visibility: None,
            language: None,
        })),
        entity_id: Some("fixture:refresh-index".into()),
        container: None,
        trace: None,
        query_groups: Vec::new(),
        selection_reason: None,
        coverage_group: None,
    };
    let modes = match matched_by {
        MatchedBy::Fts => vec![ContextRouteMode::Fts],
        MatchedBy::Vector => vec![ContextRouteMode::Vector],
        MatchedBy::FtsAndVector | MatchedBy::Lexical => {
            vec![ContextRouteMode::Fts, ContextRouteMode::Vector]
        }
    };
    ContextResult {
        query: "refresh index".into(),
        freshness: None,
        background_refresh: None,
        indexing: None,
        root: root.into(),
        source: ContextSource::Index,
        coverage: ContextCoverage::RankedSample,
        workspace_index: Some(ContextWorkspaceIndex {
            root: root.into(),
            path: root.join(".zvec-grep"),
        }),
        items: vec![item],
        group_results: Vec::new(),
        diagnostics: ContextDiagnostics {
            index: Some(IndexDiagnostics {
                hits_returned: 1,
                query_groups: Vec::new(),
                routes: modes
                    .into_iter()
                    .enumerate()
                    .map(|(index, mode)| IndexRouteDiagnostics {
                        id: format!("R{}", index + 1),
                        mode,
                        query: "refresh index".into(),
                    })
                    .collect(),
            }),
            ..ContextDiagnostics::default()
        },
    }
}

fn context_card(
    id: &'static str,
    title: &'static str,
    command: &'static str,
    result: &ContextResult,
    output_options: OutputOptions,
) -> io::Result<Fixture> {
    render(id, title, command, |output| {
        zg_cli::write_context_with_options(output, result, output_options, true)
    })
}

fn grouped(mut result: ContextResult) -> ContextResult {
    result.group_results.push(ContextGroupResult {
        id: "Q1".into(),
        query: result.query.clone(),
        role: ContextQueryGroupRole::Supplemental,
        items: result.items.clone(),
    });
    result
}

fn route_fixtures(root: &Path) -> io::Result<Vec<Fixture>> {
    let mut fixtures = Vec::new();
    for (id, title, command, matched_by) in [
        (
            "query-positional",
            "Positional query · fixture",
            "zg 'refresh index'",
            MatchedBy::FtsAndVector,
        ),
        (
            "query-hybrid",
            "Hybrid query · fixture",
            "zg --hybrid 'refresh index'",
            MatchedBy::FtsAndVector,
        ),
        (
            "query-fts",
            "FTS query · fixture",
            "zg --fts 'refresh index'",
            MatchedBy::Fts,
        ),
        (
            "query-vector",
            "Vector query · fixture",
            "zg --vector 'refresh index'",
            MatchedBy::Vector,
        ),
    ] {
        fixtures.push(context_card(
            id,
            title,
            command,
            &grouped(indexed(root, matched_by)),
            options(),
        )?);
    }
    Ok(fixtures)
}

fn query_fixtures(root: &Path) -> io::Result<Vec<Fixture>> {
    let mut fixtures = route_fixtures(root)?;
    let mut groups = grouped(indexed(root, MatchedBy::FtsAndVector));
    let mut second_item = groups.items[0].clone();
    second_item.relative_path = "src/progress.rs".into();
    second_item.absolute_path = root.join("src/progress.rs");
    second_item.content =
        "pub fn update_progress(done: usize, total: usize) {\n    draw_progress(done, total);\n}\n"
            .into();
    second_item.range = source_range(&second_item.content);
    second_item.content_range = second_item.range.clone();
    second_item.metadata = None;
    groups.group_results.push(ContextGroupResult {
        id: "Q2".into(),
        query: "update progress".into(),
        role: ContextQueryGroupRole::Supplemental,
        items: vec![second_item],
    });
    fixtures.push(context_card(
        "query-groups",
        "Multiple query groups · fixture",
        "zg --hybrid 'refresh index' --hybrid 'update progress'",
        &groups,
        options(),
    )?);

    let mut fused = indexed(root, MatchedBy::FtsAndVector);
    fused.items[0].query_groups = vec![
        ContextQueryGroupMatch {
            id: "Q1".into(),
            query: "refresh index".into(),
            role: ContextQueryGroupRole::Supplemental,
            rank: 1,
            matched_by: MatchedBy::FtsAndVector,
        },
        ContextQueryGroupMatch {
            id: "Q2".into(),
            query: "progress".into(),
            role: ContextQueryGroupRole::Supplemental,
            rank: 1,
            matched_by: MatchedBy::Fts,
        },
    ];
    fixtures.push(context_card(
        "query-fuse",
        "Fused results · fixture",
        "zg --hybrid 'refresh index' --fts progress --fuse",
        &fused,
        options(),
    )?);
    let mut empty = indexed(root, MatchedBy::Fts);
    empty.query = "missing symbol".into();
    empty.items.clear();
    empty.diagnostics.index = None;
    empty.diagnostics.empty_reason = Some(EmptyReason::NoMatches);
    fixtures.push(context_card(
        "query-empty",
        "No matching results · fixture",
        "zg --fts 'missing symbol'",
        &grouped(empty.clone()),
        options(),
    )?);
    empty.query = "refresh index".into();
    empty.diagnostics.empty_reason = Some(EmptyReason::NoSearchableFiles);
    fixtures.push(context_card(
        "query-no-files",
        "No searchable files · fixture",
        "zg --fts 'refresh index'",
        &empty,
        options(),
    )?);
    Ok(fixtures)
}

fn preview_fixtures(root: &Path) -> io::Result<Vec<Fixture>> {
    let result = indexed(root, MatchedBy::FtsAndVector);
    let mut fixtures = Vec::new();
    for (id, title, command, preview) in [
        (
            "preview-none",
            "Preview none · fixture",
            "zg --fuse --preview none 'refresh index'",
            PreviewMode::None,
        ),
        (
            "preview-short",
            "Preview short (default) · fixture",
            "zg --fuse --preview short 'refresh index'",
            PreviewMode::Short,
        ),
        (
            "preview-full",
            "Preview full · fixture",
            "zg --fuse --preview full 'refresh index'",
            PreviewMode::Full,
        ),
        (
            "preview-detail",
            "Preview detail · fixture",
            "zg --fuse --preview detail 'refresh index'",
            PreviewMode::Detail,
        ),
    ] {
        fixtures.push(context_card(
            id,
            title,
            command,
            &result,
            OutputOptions {
                preview,
                ..options()
            },
        )?);
    }
    let mut traced = result;
    traced.items[0].trace = Some(SearchHitTrace {
        recall: vec![SearchRecallTrace {
            path: ContextRouteMode::Fts,
            route_id: "R1".into(),
            query: "refresh index".into(),
            found: true,
            rank: Some(1),
            score: Some(0.81),
            forced: false,
            reason: None,
        }],
        fusion: SearchFusionTrace {
            rank: 1,
            score: 0.81,
            forced: false,
        },
        ranking: None,
        final_selection: SearchFinalTrace {
            returned_by_limit: true,
            cutoff_rank: 10,
        },
    });
    fixtures.push(context_card(
        "query-trace",
        "Ranking trace · fixture",
        "zg --fuse --trace --preview none 'refresh index'",
        &traced,
        OutputOptions {
            trace: true,
            preview: PreviewMode::None,
            ..options()
        },
    )?);
    Ok(fixtures)
}

/// Preserve the order in which lookup emits stdout and stderr without adding
/// artificial headings to either stream. The card title names the combined view.
struct Combined<'a>(&'a RefCell<Vec<u8>>);

impl Write for Combined<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn lookup_card(
    id: &'static str,
    title: &'static str,
    command: &'static str,
    result: &PathLookupResult,
    preview: PreviewMode,
) -> io::Result<Fixture> {
    render(id, title, command, |output| {
        let bytes = RefCell::new(Vec::new());
        zg_cli::write_path_lookup_result(
            Combined(&bytes),
            Combined(&bytes),
            result,
            OutputOptions {
                preview,
                ..options()
            },
            true,
        )?;
        output.extend_from_slice(&bytes.into_inner());
        Ok(())
    })
}

fn current_text(root: &Path, query: &str, path: &str, content: &str) -> ContextResult {
    let mut result = indexed(root, MatchedBy::Lexical);
    result.query = query.into();
    result.source = ContextSource::Rg;
    result.coverage = ContextCoverage::RgExhaustive;
    result.workspace_index = None;
    result.diagnostics = ContextDiagnostics::default();
    let item = &mut result.items[0];
    item.kind = ContextItemKind::LexicalMatch;
    item.absolute_path = root.join(path);
    item.relative_path = path.into();
    item.content = content.into();
    item.range = source_range(content);
    item.content_range = item.range.clone();
    item.metadata = None;
    item.score = None;
    item.entity_id = None;
    if let Some(start) = content.find(query) {
        let end = start + query.len();
        let line = content[..start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1;
        let line_start = content[..start].rfind('\n').map_or(0, |index| index + 1);
        let matched = ContentRange::Text {
            start_line: line,
            end_line: line,
            start_byte_offset: start,
            end_byte_offset: end,
            start_byte_column: start - line_start,
            end_byte_column: end - line_start,
        };
        item.excerpt_range = Some(matched.clone());
        item.match_ranges = vec![matched];
    }
    result
}

fn lookup_fixtures(root: &Path) -> io::Result<Vec<Fixture>> {
    let mut files = PathsResult {
        root: root.into(),
        paths: vec![
            PathBuf::from("src/index.rs"),
            PathBuf::from("tests/index.rs"),
        ],
        truncated: false,
    };
    let mut fixtures = vec![lookup_card(
        "lookup-files",
        "Path matches · fixture",
        "zg index.rs",
        &PathLookupResult::Files(files.clone()),
        PreviewMode::Short,
    )?];
    files.paths.truncate(1);
    files.truncated = true;
    fixtures.push(lookup_card(
        "lookup-files-truncated",
        "Path limit · stdout + stderr fixture",
        "zg --limit 1 index.rs",
        &PathLookupResult::Files(files),
        PreviewMode::Short,
    )?);
    let mut text = current_text(
        root,
        "src/index.rs",
        "docs/usage.md",
        "See src/index.rs for the refresh implementation.\n",
    );
    fixtures.push(lookup_card(
        "lookup-text",
        "Literal content fallback · stdout + stderr fixture",
        "zg src/index.rs",
        &PathLookupResult::Text(Box::new(text.clone())),
        PreviewMode::Short,
    )?);
    fixtures.push(lookup_card(
        "lookup-text-detail",
        "Fallback detail · stdout + stderr fixture",
        "zg --preview detail src/index.rs",
        &PathLookupResult::Text(Box::new(text.clone())),
        PreviewMode::Detail,
    )?);
    text.coverage = ContextCoverage::RgTruncated;
    fixtures.push(lookup_card(
        "lookup-text-truncated",
        "Incomplete text coverage · stdout + stderr fixture",
        "zg --limit 1 src/index.rs",
        &PathLookupResult::Text(Box::new(text.clone())),
        PreviewMode::Short,
    )?);
    text.items.clear();
    text.query = "src/missing.rs".into();
    text.coverage = ContextCoverage::RgExhaustive;
    text.diagnostics.empty_reason = Some(EmptyReason::NoMatches);
    fixtures.push(lookup_card(
        "lookup-empty",
        "Path and text have no matches · stdout + stderr fixture",
        "zg src/missing.rs",
        &PathLookupResult::Text(Box::new(text.clone())),
        PreviewMode::Short,
    )?);
    text.diagnostics.empty_reason = Some(EmptyReason::NoSearchableFiles);
    fixtures.push(lookup_card(
        "lookup-no-files",
        "Fallback has no searchable files · stdout + stderr fixture",
        "zg src/missing.rs",
        &PathLookupResult::Text(Box::new(text)),
        PreviewMode::Detail,
    )?);
    Ok(fixtures)
}

fn lexical_fixtures(root: &Path) -> io::Result<Vec<Fixture>> {
    let mut result = current_text(root, "progress", "src/progress.rs", "progress.advance();\n");
    let mut fixtures = vec![context_card(
        "rg-matches",
        "Managed rg matches · fixture",
        "zg --rg -n progress src/progress.rs",
        &result,
        options(),
    )?];
    result.items.clear();
    result.diagnostics.empty_reason = Some(EmptyReason::NoMatches);
    fixtures.push(context_card(
        "rg-empty",
        "Managed rg no matches · fixture",
        "zg --rg missing_symbol src/progress.rs",
        &result,
        options(),
    )?);
    let paths = [
        PathBuf::from("src/index.rs"),
        PathBuf::from("src/progress.rs"),
        PathBuf::from("docs/usage.md"),
    ];
    fixtures.push(render(
        "files-newline",
        "File listing, newline separated · fixture",
        "zg --rg --files",
        |output| zg_cli::write_file_paths(output, &paths, false),
    )?);
    fixtures.push(render(
        "files-nul",
        "File listing, raw NUL separators · fixture",
        "zg --rg --files -0",
        |output| zg_cli::write_file_paths(output, &paths, true),
    )?);
    fixtures.push(render(
        "files-empty",
        "Empty file listing (zero bytes) · fixture",
        "zg --rg --files -g '*.missing'",
        |output| zg_cli::write_file_paths(output, &[], false),
    )?);
    Ok(fixtures)
}
