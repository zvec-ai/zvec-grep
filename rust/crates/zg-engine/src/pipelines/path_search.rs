//! Current-file lookup and literal-source fallback shared by all CLI modes.

use std::{
    collections::BinaryHeap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use zg_host_native::PathPolicy;

use crate::{
    EngineError, EngineResult,
    api::{
        context::{
            ContextOptions,
            options::{QueryFilter, RgOptions},
            result::{ContextCoverage, ContextItem},
        },
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

const LIVE_CANDIDATE_LIMIT: usize = 200;

pub(crate) async fn lookup(
    direct: &super::direct_search::DirectSearchService,
    options: PathsOptions,
) -> EngineResult<PathLookupResult> {
    let paths = search(options.clone()).await?;
    if !paths.paths.is_empty() || paths.truncated {
        return Ok(PathLookupResult::Files(paths));
    }
    // Select file names before reading content so saved and caller filters remain an intersection.
    let files = search_files(options.clone(), false).await?;
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
                // Collect candidates before structural deduplication consumes the caller's limit.
                limit: Some(LIVE_CANDIDATE_LIMIT.max(limit.saturating_add(1))),
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
        )
        .await?;
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
        rg.truncated = result.coverage == ContextCoverage::RgTruncated;
    }
    Ok(PathLookupResult::Text(Box::new(result)))
}

pub(crate) async fn search(options: PathsOptions) -> EngineResult<PathsResult> {
    search_files(options, true).await
}

async fn search_files(options: PathsOptions, matching_names: bool) -> EngineResult<PathsResult> {
    tokio::task::spawn_blocking(move || search_sync(&options, matching_names))
        .await
        .map_err(|error| EngineError::internal(format!("path search task failed: {error}")))?
}

fn search_sync(options: &PathsOptions, matching_names: bool) -> EngineResult<PathsResult> {
    check_cancelled(options)?;
    let query = normalize_query(options)?;
    let (root, scan) = workspace_scan(options.root.as_deref())?;
    let policy = Arc::new(ScanPolicy::for_current_source(&root, &scan)?);
    let matcher = GlobMatcher::new(&root, &options.filter.globs)?;
    let types = crate::file_selection::file_types(
        &options.filter.file_types,
        &options.filter.excluded_file_types,
    )?;
    let format = compile_format_filter(&options.filter);
    let policy_error = Arc::new(Mutex::new(None));
    let entry_error = Arc::clone(&policy_error);
    let signal = options.signal.clone();
    let mut walker = ignore::WalkBuilder::new(&root);
    walker
        .standard_filters(false)
        .follow_links(scan.follow_symlinks)
        .max_depth(scan.max_depth)
        .max_filesize(scan.max_file_size_bytes)
        .filter_entry(move |entry| {
            if signal
                .as_ref()
                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
            {
                return false;
            }
            let selected = if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                policy.can_descend(entry.path())
            } else {
                policy.includes_file(entry.path())
            };
            selected.unwrap_or_else(|error| {
                *entry_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
                false
            })
        });
    let limit = if matching_names {
        options.limit.unwrap_or(10)
    } else {
        usize::MAX
    };
    let mut selected_paths = BinaryHeap::new();
    let mut truncated = false;
    for entry in walker.build() {
        check_cancelled(options)?;
        let entry = entry.map_err(|error| EngineError::storage_failure(error.to_string()))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(&root)
            .expect("walked path within root");
        let priority = if matching_names {
            path_priority(&query, relative, entry.path())
        } else {
            Some(0)
        };
        let Some(priority) = priority else {
            continue;
        };
        if !matcher.matches_path(relative)
            || types.matched(relative, false).is_ignore()
            || format.as_ref().is_some_and(|predicate| {
                relative
                    .file_name()
                    .is_none_or(|name| !matches_file_name(predicate, name))
            })
        {
            continue;
        }
        if !matches_modified(&entry, &options.filter) {
            continue;
        }
        selected_paths.push((priority, relative.to_path_buf()));
        if selected_paths.len() > limit {
            selected_paths.pop();
            truncated = true;
        }
    }
    check_cancelled(options)?;
    if let Some(error) = policy_error
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        return Err(super::indexing::pipeline::map_host_error(error));
    }
    Ok(PathsResult {
        root,
        paths: selected_paths
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

fn matches_modified(entry: &ignore::DirEntry, filter: &QueryFilter) -> bool {
    if filter.modified_after_epoch_ms.is_none() && filter.modified_before_epoch_ms.is_none() {
        return true;
    }
    let modified = entry
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    filter
        .modified_after_epoch_ms
        .is_none_or(|after| modified.is_some_and(|modified| modified >= after))
        && filter
            .modified_before_epoch_ms
            .is_none_or(|before| modified.is_some_and(|modified| modified <= before))
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
                name: "paths".to_owned(),
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
        ]);
        save_scan(
            root.path(),
            ScanRules {
                globs: vec![GlobRule::from("src/**")],
                ..ScanRules::default()
            },
        );
        let mut request = options(&root.path().join("src"), "auth.ts");
        request.limit = Some(1);
        request.filter = QueryFilter {
            globs: vec![GlobRule::from("src/z/**")],
            file_types: vec!["ts".to_owned()],
            formats: vec![FileFormat::TypeScript],
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
        let root = fixture(&["src/notes.md", "src/decoy.md", "excluded/notes.md"]);
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
        save_scan(
            root.path(),
            ScanRules {
                globs: vec![GlobRule::from("src/**")],
                ..ScanRules::default()
            },
        );
        let engine = ZvecGrep::new();
        let mut request = options(root.path(), "src/auth.ts");
        request.filter.formats = vec![FileFormat::Markdown];
        request.filter.categories = vec![FileCategory::Document];
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
