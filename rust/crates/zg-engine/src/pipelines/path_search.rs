//! Current-file lookup and literal-source fallback shared by all CLI modes.

use std::{
    collections::BinaryHeap,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    EngineError, EngineResult,
    api::{
        context::{
            ContextOptions, ContextResult,
            options::RgOptions,
            result::{ContextCoverage, ContextItem},
        },
        files::FilesOptions,
        paths::{PathLookupResult, PathsOptions, PathsResult},
    },
    domain::ScanRules,
    file_selection::{GlobMatcher, ScanPolicy},
    workspace::{
        layout::{find_nearest_workspace, resolve_workspace_root},
        manifest::read_workspace_manifest,
    },
};

use super::indexed_search::format_filter::{compile_format_filter, matches_file_name};

pub(crate) async fn lookup(
    direct: &super::direct_search::DirectSearchService,
    options: PathsOptions,
) -> EngineResult<PathLookupResult> {
    check_cancelled(&options)?;
    let query = normalize_query(&options)?;
    // Select file names before reading content so saved and caller filters remain an intersection.
    let files = selected_files(&options).await?;
    let paths = matching_paths(&options, &files, &query)?;
    if !paths.paths.is_empty() || paths.truncated {
        return Ok(PathLookupResult::Files(paths));
    }
    let result = literal_fallback(direct, options, files).await?;
    Ok(PathLookupResult::Text(Box::new(result)))
}

pub(crate) async fn search(options: PathsOptions) -> EngineResult<PathsResult> {
    check_cancelled(&options)?;
    let query = normalize_query(&options)?;
    let files = selected_files(&options).await?;
    matching_paths(&options, &files, &query)
}

async fn literal_fallback(
    direct: &super::direct_search::DirectSearchService,
    options: PathsOptions,
    files: PathsResult,
) -> EngineResult<ContextResult> {
    // Preserve the original spelling for literal source matches, including path separators.
    let query = options.query.trim().to_owned();
    let ignore_case = query == query.to_lowercase();
    let limit = options.limit.unwrap_or(10);
    let mut result = direct
        .context_selected_files(
            ContextOptions {
                query: Some(query),
                root: Some(files.root),
                rg: true,
                auto_update: false,
                // Apply the result limit only after structural deduplication.
                limit: None,
                hidden: true,
                no_ignore: true,
                follow: true,
                rg_options: RgOptions {
                    fixed_strings: true,
                    ignore_case,
                    before_context: 2,
                    after_context: 2,
                    ..RgOptions::default()
                },
                signal: options.signal,
                ..ContextOptions::default()
            },
            files.paths,
            // Each matched file contributes at least one result. Enrich enough files
            // for the requested results plus one extra result to detect truncation.
            limit.saturating_add(1),
        )
        .await?;
    deduplicate_and_limit(&mut result, limit);
    Ok(result)
}

fn deduplicate_and_limit(result: &mut ContextResult, limit: usize) {
    let mut selected: Vec<ContextItem> = Vec::new();
    for item in std::mem::take(&mut result.items) {
        let target = item
            .container
            .as_ref()
            .map_or(&item.range, |value| &value.range);
        if selected.iter().any(|existing| {
            existing.relative_path == item.relative_path
                && existing
                    .container
                    .as_ref()
                    .map_or(&existing.range, |value| &value.range)
                    == target
        }) {
            continue;
        }
        selected.push(item);
        if selected.len() > limit {
            break;
        }
    }
    if selected.len() > limit {
        result.coverage = ContextCoverage::RgTruncated;
    }
    selected.truncate(limit);
    for (index, item) in selected.iter_mut().enumerate() {
        item.rank = index + 1;
    }
    result.items = selected;
    if let Some(rg) = &mut result.diagnostics.rg {
        rg.limit = Some(limit);
        rg.truncated = result.coverage == ContextCoverage::RgTruncated;
    }
}

async fn selected_files(options: &PathsOptions) -> EngineResult<PathsResult> {
    let (root, scan) = workspace_scan(options.root.as_deref())?;
    let policy = Arc::new(ScanPolicy::for_current_source(&root, &scan)?);
    let matcher = GlobMatcher::new(&root, &options.filter.globs)?;
    let format = compile_format_filter(&options.filter);
    let files = crate::lexical::files::list_with_policy(
        FilesOptions {
            root: Some(root.clone()),
            // Workspace policy owns ignore and hidden selection for automatic lookup.
            hidden: true,
            no_ignore: true,
            follow: scan.follow_symlinks(),
            max_depth: scan.max_depth(),
            max_file_size_bytes: scan.max_file_size_bytes(),
            file_types: options.filter.file_types.clone(),
            excluded_file_types: options.filter.excluded_file_types.clone(),
            modified_after_epoch_ms: options.filter.modified_after_epoch_ms,
            modified_before_epoch_ms: options.filter.modified_before_epoch_ms,
            signal: options.signal.clone(),
            ..FilesOptions::default()
        },
        policy,
    )
    .await?;
    if !files.errors.is_empty() {
        return Err(EngineError::storage_failure(files.errors.join("\n")));
    }
    check_cancelled(options)?;
    let paths = files
        .paths
        .into_iter()
        .filter(|relative| {
            matcher.matches_path(relative)
                && format.as_ref().is_none_or(|predicate| {
                    relative
                        .file_name()
                        .is_some_and(|name| matches_file_name(predicate, name))
                })
        })
        .collect();
    Ok(PathsResult {
        root,
        paths,
        truncated: false,
    })
}

fn matching_paths(
    options: &PathsOptions,
    files: &PathsResult,
    query: &str,
) -> EngineResult<PathsResult> {
    let limit = options.limit.unwrap_or(10);
    let mut paths = BinaryHeap::new();
    let mut truncated = false;
    for relative in &files.paths {
        check_cancelled(options)?;
        if let Some(priority) = path_priority(query, relative, &files.root.join(relative)) {
            paths.push((priority, relative.clone()));
            if paths.len() > limit {
                paths.pop();
                truncated = true;
            }
        }
    }
    Ok(PathsResult {
        root: files.root.clone(),
        paths: paths
            .into_sorted_vec()
            .into_iter()
            .map(|(_, path)| path)
            .collect(),
        truncated,
    })
}

fn workspace_scan(root: Option<&Path>) -> EngineResult<(PathBuf, ScanRules)> {
    let requested = resolve_workspace_root(root)?;
    let location = find_nearest_workspace(&requested)?;
    let manifest = location
        .as_ref()
        .map(|location| read_workspace_manifest(&location.home))
        .transpose()?
        .flatten();
    let root = std::fs::canonicalize(location.as_ref().map_or(requested.as_path(), |l| &l.root))
        .map_err(|error| EngineError::from_io("resolve path search root", &error))?;
    let scan = manifest.map_or_else(ScanRules::default, |manifest| manifest.workspace.scan);
    Ok((root, scan))
}

fn normalize_query(options: &PathsOptions) -> EngineResult<String> {
    let query = normalize_path(options.query.trim());
    if query.is_empty() || !options.filter.symbol_types.is_empty() {
        return Err(EngineError::invalid_argument(
            "path lookup requires a non-empty path and does not support symbol filters",
        ));
    }
    let path = Path::new(&query);
    // Resolve directory aliases while preserving the queried file's own name.
    if path.is_absolute()
        && let Some(name) = path.file_name()
        && let Some(parent) = path
            .parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
        && let Some(path) = parent.join(name).to_str()
    {
        return Ok(normalize_path(path));
    }
    Ok(query)
}

fn normalize_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    if let Some(tail) = path.strip_prefix("//?/UNC/") {
        return format!("//{tail}");
    }
    path.trim_start_matches("//?/")
        .trim_start_matches("./")
        .to_owned()
}

fn path_priority(query: &str, relative: &Path, absolute: &Path) -> Option<u8> {
    let relative = normalize_path(relative.to_str()?);
    if relative == query
        || absolute
            .to_str()
            .is_some_and(|path| normalize_path(path) == query)
    {
        Some(0)
    } else {
        relative
            .strip_suffix(query)
            .filter(|prefix| prefix.ends_with('/'))
            .map(|_| 1)
    }
}

fn check_cancelled(options: &PathsOptions) -> EngineResult<()> {
    if options
        .signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        return Err(EngineError::cancelled("path search was cancelled"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::PathBuf};

    use super::*;
    use crate::{
        ZvecGrep,
        api::context::options::{FileCategory, FileFormat, GlobRule, QueryFilter},
        api::index::options::ScanRulesUpdate,
        domain::{IndexState, ScanRules, Workspace},
        workspace::manifest::{WorkspaceManifest, write_workspace_manifest},
    };

    fn fixture(files: &[&str]) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("workspace");
        for file in files {
            let path = root.path().join(file);
            fs::create_dir_all(path.parent().expect("parent")).expect("directories");
            fs::write(path, b"").expect("empty fixture file");
        }
        root
    }

    fn options(root: &Path, query: &str) -> PathsOptions {
        PathsOptions {
            query: query.to_owned(),
            root: Some(root.to_path_buf()),
            ..PathsOptions::default()
        }
    }

    fn save_scan(root: &Path, scan: ScanRules) {
        let root = fs::canonicalize(root).expect("canonical root");
        let home = root.join(".zvec-grep");
        let manifest = WorkspaceManifest::new(
            Workspace {
                root,
                scan,
                index: IndexState::Uninitialized,
                created_epoch_ms: 0,
                updated_epoch_ms: 0,
            },
            home.clone(),
            None,
            BTreeMap::default(),
        )
        .expect("manifest");
        write_workspace_manifest(&home, &manifest).expect("save scan rules");
    }

    #[tokio::test]
    async fn exact_paths_precede_suffixes_and_never_match_partial_names_or_contents() {
        let root = fixture(&[
            "src/auth.ts",
            "a/src/auth.ts",
            "auth.ts",
            "otherauth.ts",
            "text.ts",
        ]);
        fs::write(root.path().join("text.ts"), "src/auth.ts").expect("content decoy");
        let engine = ZvecGrep::new();
        let result = engine
            .paths(options(root.path(), "auth.ts"))
            .await
            .expect("paths");
        assert_eq!(
            result.paths,
            ["auth.ts", "a/src/auth.ts", "src/auth.ts"].map(PathBuf::from)
        );
        assert!(!result.truncated);
        for query in ["src/auth.ts", "./src/auth.ts", ".\\src\\auth.ts"] {
            let result = engine
                .paths(options(root.path(), query))
                .await
                .expect("paths");
            assert_eq!(
                result.paths,
                ["src/auth.ts", "a/src/auth.ts"].map(PathBuf::from)
            );
        }
        let absolute = root.path().join("src/auth.ts");
        let result = engine
            .paths(options(root.path(), &absolute.to_string_lossy()))
            .await
            .expect("absolute lookup");
        assert_eq!(result.paths, [PathBuf::from("src/auth.ts")]);
        assert_eq!(engine.runtime_snapshot().loaded_models, 0);
        assert!(!root.path().join(".zvec-grep").exists());
        engine.close();
    }

    #[tokio::test]
    async fn uses_current_ignore_rules_and_always_excludes_internal_directories() {
        let root = fixture(&[
            "src/auth.ts",
            "ignored/auth.ts",
            "node_modules/auth.ts",
            ".hidden/auth.ts",
            ".git/auth.ts",
            ".zvec-grep/auth.ts",
        ]);
        fs::write(root.path().join(".gitignore"), "ignored/\n").expect("ignore rules");
        let result = search(options(root.path(), "auth.ts"))
            .await
            .expect("paths");
        assert_eq!(
            result.paths,
            ["node_modules/auth.ts", "src/auth.ts"].map(PathBuf::from)
        );
        fs::create_dir(root.path().join("new")).expect("new directory");
        fs::write(root.path().join("new/auth.ts"), b"").expect("new file");
        fs::remove_file(root.path().join("src/auth.ts")).expect("delete file");
        let result = search(options(root.path(), "auth.ts"))
            .await
            .expect("current paths");
        assert_eq!(
            result.paths,
            ["new/auth.ts", "node_modules/auth.ts"].map(PathBuf::from)
        );
    }

    #[tokio::test]
    async fn file_names_include_artifacts_without_reading_or_embedding_them() {
        let root = fixture(&[
            "Cargo.lock",
            "picture.png",
            "dist/auth.ts",
            "vendor/auth.ts",
        ]);
        for file in [
            "Cargo.lock",
            "picture.png",
            "dist/auth.ts",
            "vendor/auth.ts",
        ] {
            fs::write(root.path().join(file), b"\xff\x00").expect("binary fixture");
        }
        let engine = ZvecGrep::new();
        for (query, expected) in [
            ("Cargo.lock", vec!["Cargo.lock"]),
            ("picture.png", vec!["picture.png"]),
            ("auth.ts", vec!["dist/auth.ts", "vendor/auth.ts"]),
        ] {
            let PathLookupResult::Files(result) = engine
                .lookup_path(options(root.path(), query))
                .await
                .expect("file lookup")
            else {
                panic!("matching names must not fall back to content search");
            };
            assert_eq!(
                result.paths,
                expected.into_iter().map(PathBuf::from).collect::<Vec<_>>()
            );
        }
        assert_eq!(engine.runtime_snapshot().loaded_models, 0);
        assert!(!root.path().join(".zvec-grep").exists());
        engine.close();
    }

    #[tokio::test]
    async fn saved_scan_and_query_filters_intersect_before_limiting() {
        let root = fixture(&[
            "excluded/auth.ts",
            "src/a/auth.ts",
            "src/z/auth.ts",
            "src/z/noauth.ts",
            "src/z/notes.md",
            "src/z/tsconfig.json",
        ]);
        save_scan(
            root.path(),
            ScanRulesUpdate {
                globs: Some(vec![GlobRule::from("src/**")]),
                included_formats: Some(vec![FileFormat::TypeScript]),
                excluded_formats: Some(vec![FileFormat::Json]),
                ..ScanRulesUpdate::default()
            }
            .apply(&ScanRules::default())
            .expect("scan rules"),
        );
        for query in ["notes.md", "tsconfig.json"] {
            assert!(
                search(options(root.path(), query))
                    .await
                    .expect("saved format restriction")
                    .paths
                    .is_empty()
            );
        }
        let mut request = options(&root.path().join("src"), "auth.ts");
        request.limit = Some(1);
        request.filter = QueryFilter {
            globs: vec![GlobRule::from("src/z/**")],
            file_types: vec!["ts".to_owned()],
            included_formats: vec![FileFormat::TypeScript],
            categories: vec![FileCategory::Code],
            modified_after_epoch_ms: Some(0),
            ..QueryFilter::default()
        };
        let result = search(request.clone()).await.expect("filtered lookup");
        assert_eq!(
            result.root,
            fs::canonicalize(root.path()).expect("workspace root")
        );
        assert_eq!(result.paths, [PathBuf::from("src/z/auth.ts")]);
        assert!(!result.truncated);
        request.filter.excluded_file_types.push("ts".to_owned());
        assert!(
            search(request.clone())
                .await
                .expect("type exclusion")
                .paths
                .is_empty()
        );
        request.filter.excluded_file_types.clear();
        request.filter.excluded_formats.push(FileFormat::TypeScript);
        assert!(
            search(request.clone())
                .await
                .expect("format exclusion")
                .paths
                .is_empty()
        );
        request.filter.excluded_formats.clear();
        request.filter.modified_before_epoch_ms = Some(0);
        assert!(
            search(request)
                .await
                .expect("time exclusion")
                .paths
                .is_empty()
        );
    }

    #[tokio::test]
    async fn reports_limits_without_confusing_empty_selection_with_no_matches() {
        let root = fixture(&["src/auth.ts", "auth.ts"]);
        for limit in [0, 1, 2] {
            let mut request = options(root.path(), "auth.ts");
            request.limit = Some(limit);
            let result = search(request).await.expect("limited paths");
            assert_eq!(
                result.paths,
                ["auth.ts", "src/auth.ts"].map(PathBuf::from)[..limit]
            );
            assert_eq!(result.truncated, limit < 2);
        }
        assert!(
            search(options(root.path(), "missing.ts"))
                .await
                .expect("no matches")
                .paths
                .is_empty()
        );
    }

    #[tokio::test]
    async fn literal_fallback_intersects_saved_and_caller_filters_without_a_model() {
        let root = fixture(&[
            "src/notes.md",
            "src/decoy.md",
            "src/notes.txt",
            "src/tsconfig.json",
            "excluded/notes.md",
        ]);
        fs::write(
            root.path().join("src/notes.md"),
            "Import src/auth.ts here.\n",
        )
        .expect("reference");
        fs::write(
            root.path().join("src/decoy.md"),
            "Import src/authXts here.\n",
        )
        .expect("decoy");
        fs::write(
            root.path().join("excluded/notes.md"),
            "Import src/auth.ts here.\n",
        )
        .expect("excluded reference");
        fs::write(
            root.path().join("src/notes.txt"),
            "Import src/auth.ts here.\n",
        )
        .expect("excluded text format");
        fs::write(
            root.path().join("src/tsconfig.json"),
            "{\"reference\":\"src/auth.ts\"}\n",
        )
        .expect("excluded overlapping format");
        save_scan(
            root.path(),
            ScanRulesUpdate {
                globs: Some(vec![GlobRule::from("src/**")]),
                included_formats: Some(vec![FileFormat::Markdown, FileFormat::TypeScript]),
                excluded_formats: Some(vec![FileFormat::Json]),
                ..ScanRulesUpdate::default()
            }
            .apply(&ScanRules::default())
            .expect("scan rules"),
        );
        let engine = ZvecGrep::new();
        let mut request = options(root.path(), "src/auth.ts");
        request.filter.included_formats = vec![
            FileFormat::Markdown,
            FileFormat::TypeScript,
            FileFormat::Text,
        ];
        request.filter.categories = vec![FileCategory::Document, FileCategory::Code];
        request.limit = Some(1);
        let PathLookupResult::Text(result) = engine
            .lookup_path(request.clone())
            .await
            .expect("text fallback")
        else {
            panic!("no file has the queried name");
        };
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].relative_path, PathBuf::from("src/notes.md"));
        assert!(result.items[0].content.contains("src/auth.ts"));
        assert_eq!(result.coverage, ContextCoverage::RgExhaustive);
        request.filter.globs = vec![GlobRule::from("excluded/**")];
        let PathLookupResult::Text(result) = engine
            .lookup_path(request)
            .await
            .expect("empty intersection")
        else {
            panic!("no file has the queried name");
        };
        assert!(result.items.is_empty());
        assert_eq!(
            result.coverage,
            crate::api::context::result::ContextCoverage::RgExhaustive
        );
        let PathLookupResult::Files(result) = engine
            .lookup_path(options(root.path(), "notes.md"))
            .await
            .expect("file lookup")
        else {
            panic!("file names take precedence over source contents");
        };
        assert_eq!(result.paths, [PathBuf::from("src/notes.md")]);
        assert_eq!(engine.runtime_snapshot().loaded_models, 0);
        engine.close();
    }

    #[tokio::test]
    async fn literal_fallback_limits_unique_containers_after_more_than_200_matches() {
        let root = fixture(&["refs.md"]);
        let source = format!(
            "# Repeated\n\n{}\n# Second\n\na.h\n\n# Third\n\na.h\n",
            "a.h\n".repeat(210)
        );
        fs::write(root.path().join("refs.md"), source).expect("repeated references");
        let engine = ZvecGrep::new();
        for limit in [3, 2, 0] {
            let mut request = options(root.path(), "a.h");
            request.limit = Some(limit);
            let PathLookupResult::Text(result) =
                engine.lookup_path(request).await.expect("literal fallback")
            else {
                panic!("no file has the queried name");
            };
            assert_eq!(result.items.len(), limit);
            assert_eq!(
                result.coverage,
                if limit < 3 {
                    ContextCoverage::RgTruncated
                } else {
                    ContextCoverage::RgExhaustive
                }
            );
            assert_eq!(
                result
                    .diagnostics
                    .rg
                    .as_ref()
                    .expect("rg diagnostics")
                    .truncated,
                limit < 3
            );
            assert_eq!(
                result
                    .diagnostics
                    .rg
                    .as_ref()
                    .expect("rg diagnostics")
                    .limit,
                Some(limit)
            );
            for (index, item) in result.items.iter().enumerate() {
                assert_eq!(item.rank, index + 1);
            }
        }
        engine.close();
    }

    #[tokio::test]
    async fn literal_fallback_deduplicates_entities_after_more_than_100_files() {
        for (extension, source) in [
            (
                "ts",
                "function load() {\n  // auth.ts\n  // before\n  // middle\n  // after\n",
            ),
            ("md", "# Reference\n\nauth.ts\n\npadding\n\n"),
        ] {
            let root = tempfile::tempdir().expect("workspace");
            for index in 0..101 {
                let repeated = if index == 100 { "auth.ts\n" } else { "" };
                let content = if extension == "ts" {
                    format!("{source}  // {repeated}\n}}\n")
                } else {
                    format!("{source}\n{repeated}")
                };
                fs::write(root.path().join(format!("{index:03}.{extension}")), content)
                    .expect("source references");
            }
            let engine = ZvecGrep::new();
            let direct = engine
                .context(ContextOptions {
                    root: Some(root.path().to_path_buf()),
                    query: Some("auth.ts".to_owned()),
                    rg: true,
                    rg_options: RgOptions {
                        fixed_strings: true,
                        ..RgOptions::default()
                    },
                    ..ContextOptions::default()
                })
                .await
                .expect("ordinary rg retains its enrichment budget");
            let structure = direct.diagnostics.structure.expect("structure diagnostics");
            assert_eq!(structure.file_limit, 100);
            assert_eq!(structure.enriched_files, 100);
            assert!(structure.truncated);
            assert!(direct.items.last().expect("last match").container.is_none());

            for limit in [101, 100, 102, 0, 1, usize::MAX] {
                let mut request = options(root.path(), "auth.ts");
                request.limit = Some(limit);
                let PathLookupResult::Text(result) = engine
                    .lookup_path(request)
                    .await
                    .expect("lookup beyond the ordinary rg enrichment budget")
                else {
                    panic!("no file has the queried name");
                };
                assert_eq!(result.items.len(), limit.min(101), "{extension}, {limit}");
                assert_eq!(
                    result.coverage,
                    if limit < 101 {
                        ContextCoverage::RgTruncated
                    } else {
                        ContextCoverage::RgExhaustive
                    },
                    "{extension}, {limit}: repeated matches must not imply more entities"
                );
                assert_eq!(
                    result
                        .diagnostics
                        .rg
                        .as_ref()
                        .expect("rg diagnostics")
                        .truncated,
                    limit < 101
                );
                assert!(result.items.iter().all(|item| item.container.is_some()));
                if limit >= 101 {
                    let structure = result
                        .diagnostics
                        .structure
                        .as_ref()
                        .expect("structure diagnostics");
                    assert_eq!(structure.enriched_files, 101);
                    assert!(!structure.truncated);
                }
            }
            engine.close();
        }
    }

    #[tokio::test]
    async fn literal_fallback_counts_fragmented_functions_and_sections_once() {
        for (file, source) in [
            (
                "refs.ts",
                format!(
                    "function load() {{\n  // src/auth.ts\n{}  // src/auth.ts\n}}\nfunction other() {{\n  // src/auth.ts\n}}\n",
                    "  // padding keeps the matches in separate retrieval fragments\n".repeat(100)
                ),
            ),
            (
                "refs.md",
                format!(
                    "# Repeated\n\nsrc/auth.ts\n\n{}\nsrc/auth.ts\n\n# Repeated\n\nsrc/auth.ts\n",
                    "Padding keeps the matches in separate retrieval fragments.\n".repeat(100)
                ),
            ),
        ] {
            let root = fixture(&[file]);
            fs::write(root.path().join(file), &source).expect("fragmented source");
            let last_match = source.rfind("src/auth.ts").expect("final reference");
            let last_line = source[..last_match]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count()
                + 1;
            let engine = ZvecGrep::new();
            let direct = engine
                .context(ContextOptions {
                    root: Some(root.path().to_path_buf()),
                    query: Some("src/auth.ts".to_owned()),
                    rg: true,
                    rg_options: RgOptions {
                        fixed_strings: true,
                        ..RgOptions::default()
                    },
                    ..ContextOptions::default()
                })
                .await
                .expect("ordinary rg keeps individual matches");
            assert_eq!(direct.items.len(), 3, "{file}");
            assert_ne!(
                direct.items[0]
                    .container
                    .as_ref()
                    .expect("first fragment")
                    .range,
                direct.items[1]
                    .container
                    .as_ref()
                    .expect("second fragment")
                    .range,
                "{file}: ordinary rg retains fragment containers"
            );
            let mut request = options(root.path(), "src/auth.ts");
            request.limit = Some(2);
            let PathLookupResult::Text(result) =
                engine.lookup_path(request).await.expect("literal fallback")
            else {
                panic!("no file has the queried name");
            };
            assert_eq!(result.items.len(), 2, "{file}");
            assert_eq!(
                result.items[1]
                    .excerpt_range
                    .as_ref()
                    .expect("matched source")
                    .start_line(),
                Some(last_line),
                "{file}: later entities must not be displaced by another fragment"
            );
            assert_eq!(result.coverage, ContextCoverage::RgExhaustive, "{file}");
            engine.close();
        }
    }

    #[tokio::test]
    async fn literal_fallback_preserves_original_path_spelling_and_smart_case() {
        let root = fixture(&["normalized.txt", "literal.txt", "uppercase.txt"]);
        for (file, content) in [
            ("normalized.txt", "src/auth.ts"),
            ("literal.txt", r".\src\auth.ts"),
            ("uppercase.txt", r".\SRC\Auth.ts"),
        ] {
            fs::write(root.path().join(file), content).expect("literal reference");
        }
        let engine = ZvecGrep::new();
        for (query, expected) in [
            (r" .\src\auth.ts ", vec!["literal.txt", "uppercase.txt"]),
            (r" .\SRC\Auth.ts ", vec!["uppercase.txt"]),
        ] {
            let PathLookupResult::Text(result) = engine
                .lookup_path(options(root.path(), query))
                .await
                .expect("literal fallback")
            else {
                panic!("no file has the queried name");
            };
            assert_eq!(result.query, query.trim());
            assert_eq!(
                result
                    .items
                    .iter()
                    .map(|item| item.relative_path.clone())
                    .collect::<Vec<_>>(),
                expected.into_iter().map(PathBuf::from).collect::<Vec<_>>()
            );
            assert_eq!(result.coverage, ContextCoverage::RgExhaustive);
        }
        engine.close();
    }

    #[tokio::test]
    async fn invalid_ignore_rules_do_not_report_a_complete_lookup() {
        let root = fixture(&["auth.ts", "notes.md"]);
        fs::write(root.path().join(".ignore"), "{invalid\n").expect("invalid ignore rule");
        fs::write(root.path().join("notes.md"), "missing.ts").expect("literal reference");
        let engine = ZvecGrep::new();
        for query in ["auth.ts", "missing.ts"] {
            assert!(
                engine
                    .lookup_path(options(root.path(), query))
                    .await
                    .is_err()
            );
        }
        assert_eq!(engine.runtime_snapshot().loaded_models, 0);
        assert!(!root.path().join(".zvec-grep").exists());
        engine.close();
    }

    #[tokio::test]
    async fn cancellation_and_closed_engine_reject_lookup() {
        let root = fixture(&["auth.ts"]);
        let mut request = options(root.path(), "auth.ts");
        let signal = tokio_util::sync::CancellationToken::new();
        signal.cancel();
        request.signal = Some(signal);
        assert_eq!(
            search(request).await.expect_err("cancelled").code(),
            EngineError::CANCELLED
        );
        let engine = ZvecGrep::new();
        engine.close();
        assert!(engine.paths(options(root.path(), "auth.ts")).await.is_err());
    }
}
