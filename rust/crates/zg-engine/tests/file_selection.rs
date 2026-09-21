mod support;

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

use serde_json::{Value, json};
use support::{
    EmbeddingServer, configure_remote_model, index_options, info_options, native_file_records,
};
use zg_engine::{
    ZvecGrep,
    api::{
        context::{
            ContextOptions,
            options::{
                ContextRoute, ContextRouteMode, FileCategory, FileFormat, QueryFilter, RgOptions,
                SymbolType,
            },
            result::EntityMetadata,
        },
        index::{
            IndexOptions,
            options::{GlobRule, ScanRules, ScanRulesUpdate},
        },
    },
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn write_sources(root: &Path, sources: &[(&str, &str)]) -> std::io::Result<()> {
    for (relative, contents) in sources {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture parent"))?;
        fs::write(path, contents)?;
    }
    Ok(())
}

fn paths(names: &[&str]) -> BTreeSet<PathBuf> {
    names.iter().map(PathBuf::from).collect()
}

fn stored_paths(index_path: &Path) -> TestResult<BTreeSet<PathBuf>> {
    Ok(native_file_records(index_path)?
        .iter()
        .map(|file| {
            PathBuf::from(
                file["relative_path"]["value"]
                    .as_str()
                    .expect("UTF-8 fixture path"),
            )
        })
        .collect())
}

async fn search_paths(
    engine: &ZvecGrep,
    root: &Path,
    mode: ContextRouteMode,
    filter: QueryFilter,
) -> TestResult<BTreeSet<PathBuf>> {
    let result = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode,
                query: "orchard".into(),
            }],
            filter,
            limit: Some(64),
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    Ok(result
        .items
        .into_iter()
        .map(|item| item.relative_path)
        .collect())
}

async fn assert_index_and_queries(
    engine: &ZvecGrep,
    root: &Path,
    filter: &QueryFilter,
    expected: &BTreeSet<PathBuf>,
) -> TestResult {
    let info = engine.info(info_options(root)).await?;
    assert_eq!(
        &stored_paths(&info.index_path)?,
        expected,
        "stored file membership"
    );
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        assert_eq!(
            &search_paths(engine, root, mode, QueryFilter::default()).await?,
            expected,
            "unfiltered {mode:?}"
        );
        assert_eq!(
            &search_paths(engine, root, mode, filter.clone()).await?,
            expected,
            "same selection in {mode:?}"
        );
    }
    assert_eq!(
        engine
            .info(info_options(root))
            .await?
            .workspace_index
            .expect("workspace")
            .scan
            .globs,
        filter.globs,
        "query filters do not alter workspace configuration"
    );
    Ok(())
}

#[tokio::test]
async fn ordered_globs_have_the_same_membership_during_indexing_and_both_query_routes() -> TestResult
{
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    write_sources(
        root,
        &[
            ("src/main.rs", "/// orchard\npub fn main() {}\n"),
            ("src/derived/keep.rs", "/// orchard\npub fn keep() {}\n"),
            (
                "src/derived/nested/keep.rs",
                "/// orchard\npub fn nested() {}\n",
            ),
            ("blocked/keep.rs", "/// orchard\npub fn blocked() {}\n"),
            ("readme.md", "# orchard\n"),
        ],
    )?;
    let filter = QueryFilter {
        globs: vec![
            GlobRule {
                pattern: "*.RS".into(),
                case_insensitive: true,
            },
            "!src/derived/**".into(),
            "src/derived/keep.rs".into(),
            "!blocked".into(),
            "blocked/keep.rs".into(),
        ],
        ..QueryFilter::default()
    };
    let engine = ZvecGrep::new();
    let indexed = engine
        .index(IndexOptions {
            scan: ScanRulesUpdate {
                globs: Some(filter.globs.clone()),
                ..ScanRulesUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((indexed.files_added, indexed.files_failed), (2, 0));
    assert_index_and_queries(
        &engine,
        root,
        &filter,
        &paths(&["src/main.rs", "src/derived/keep.rs"]),
    )
    .await?;
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
async fn nested_git_roots_are_scanned_and_explicit_globs_override_ignore_rules() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    write_sources(
        root,
        &[
            ("plain.txt", "orchard plain source"),
            ("nested/child.txt", "orchard nested source"),
            ("nested/.git/objects/private.txt", "orchard git internals"),
            (".gitignore", "ignored/\nblocked.txt\n"),
            ("blocked.txt", "orchard ignored file"),
            ("ignored/keep.txt", "orchard explicitly included subtree"),
        ],
    )?;
    let engine = ZvecGrep::new();
    assert_eq!(engine.index(index_options(root)).await?.files_added, 2);
    assert_index_and_queries(
        &engine,
        root,
        &QueryFilter::default(),
        &paths(&["plain.txt", "nested/child.txt"]),
    )
    .await?;
    let filter = QueryFilter {
        // A directory ignored by .gitignore must itself be explicitly included.
        globs: vec!["*.txt".into(), "ignored".into(), "ignored/**".into()],
        ..QueryFilter::default()
    };
    let indexed = engine
        .index(IndexOptions {
            scan: ScanRulesUpdate {
                globs: Some(filter.globs.clone()),
                ..ScanRulesUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((indexed.files_added, indexed.files_failed), (2, 0));
    assert_index_and_queries(
        &engine,
        root,
        &filter,
        &paths(&[
            "plain.txt",
            "nested/child.txt",
            "blocked.txt",
            "ignored/keep.txt",
        ]),
    )
    .await?;
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
async fn nested_git_scan_option_controls_membership_and_survives_reopen_and_rebuild() -> TestResult
{
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    write_sources(
        root,
        &[
            ("plain.txt", "orchard root source"),
            ("ordinary/keep.txt", "orchard ordinary directory"),
            (".git/objects/private.txt", "orchard root git internals"),
            ("nested/keep.txt", "orchard nested repository"),
            ("nested/ignored.txt", "orchard ignored nested source"),
            ("nested/.gitignore", "ignored.txt\n"),
            (
                "nested/.git/objects/private.txt",
                "orchard nested git internals",
            ),
            ("submodule/keep.txt", "orchard submodule source"),
            ("submodule/ignored.txt", "orchard ignored submodule source"),
            ("submodule/.gitignore", "ignored.txt\n"),
            ("submodule/.git", "gitdir: ../.git/modules/submodule\n"),
        ],
    )?;
    let boundary_filter = QueryFilter {
        globs: vec![
            "*.txt".into(),
            "nested".into(),
            "nested/**".into(),
            "submodule".into(),
            "submodule/**".into(),
        ],
        ..QueryFilter::default()
    };
    let outer = paths(&["plain.txt", "ordinary/keep.txt"]);
    let including_nested = paths(&[
        "plain.txt",
        "ordinary/keep.txt",
        "nested/keep.txt",
        "submodule/keep.txt",
    ]);
    let engine = ZvecGrep::new();
    let indexed = engine
        .index(IndexOptions {
            scan: ScanRulesUpdate {
                globs: Some(boundary_filter.globs.clone()),
                nested_git: Some(false),
                no_ignore: Some(true),
                ..ScanRulesUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((indexed.files_added, indexed.files_failed), (2, 0));
    // Neither explicit directory globs nor no_ignore may cross a disabled boundary.
    assert_index_and_queries(&engine, root, &boundary_filter, &outer).await?;

    let indexed = engine
        .index(IndexOptions {
            scan: ScanRulesUpdate {
                globs: Some(Vec::new()),
                nested_git: Some(true),
                no_ignore: Some(false),
                ..ScanRulesUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((indexed.files_added, indexed.files_failed), (2, 0));
    // Entering a repository does not disable its ignore files or expose .git internals.
    assert_index_and_queries(&engine, root, &QueryFilter::default(), &including_nested).await?;
    assert_eq!(engine.index(index_options(root)).await?.files_unchanged, 4);
    assert!(
        engine
            .info(info_options(root))
            .await?
            .workspace_index
            .expect("workspace after omitted update")
            .scan
            .nested_git
    );
    engine.close();

    let engine = ZvecGrep::new();
    let info = engine.info(info_options(root)).await?;
    assert!(
        info.workspace_index
            .expect("reopened workspace")
            .scan
            .nested_git
    );
    let manifest: Value = serde_json::from_slice(&fs::read(info.home.join("manifest.json"))?)?;
    assert_eq!(manifest["scan"]["nested_git"], true);
    let rebuilt = engine
        .index(IndexOptions {
            rebuild: true,
            ..index_options(root)
        })
        .await?;
    assert_eq!((rebuilt.files_added, rebuilt.files_failed), (4, 0));
    assert_index_and_queries(&engine, root, &QueryFilter::default(), &including_nested).await?;

    let indexed = engine
        .index(IndexOptions {
            scan: ScanRulesUpdate {
                nested_git: Some(false),
                ..ScanRulesUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((indexed.files_deleted, indexed.files_failed), (2, 0));
    assert_index_and_queries(&engine, root, &QueryFilter::default(), &outer).await?;
    assert_eq!(engine.index(index_options(root)).await?.files_unchanged, 2);
    engine.close();

    let engine = ZvecGrep::new();
    let info = engine.info(info_options(root)).await?;
    assert!(
        !info
            .workspace_index
            .expect("reopened workspace")
            .scan
            .nested_git
    );
    let manifest: Value = serde_json::from_slice(&fs::read(info.home.join("manifest.json"))?)?;
    assert_eq!(manifest["scan"]["nested_git"], false);
    let rebuilt = engine
        .index(IndexOptions {
            rebuild: true,
            ..index_options(root)
        })
        .await?;
    assert_eq!((rebuilt.files_added, rebuilt.files_failed), (2, 0));
    assert_index_and_queries(&engine, root, &QueryFilter::default(), &outer).await?;
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Verify filename filtering remains independent of extraction and source availability"
)]
async fn filename_formats_and_categories_filter_queries_without_limiting_indexing() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    write_sources(
        root,
        &[
            (
                "script",
                "#!/usr/bin/env python3\n# orchard\ndef orchard():\n    return 1\n",
            ),
            (
                "page.html",
                "<!doctype html><html><body>orchard</body></html>",
            ),
            ("note.md", "# orchard\n"),
            ("settings.json", "{\"orchard\":\"apple\"}"),
            ("source.rs", "/// orchard\npub fn source() {}\n"),
        ],
    )?;
    let filter = QueryFilter {
        formats: vec![FileFormat::Python, FileFormat::Rust, FileFormat::Json],
        categories: vec![FileCategory::Code, FileCategory::Data],
        ..QueryFilter::default()
    };
    let engine = ZvecGrep::new();
    let indexed = engine.index(index_options(root)).await?;
    assert_eq!((indexed.files_added, indexed.files_failed), (5, 0));
    assert_index_and_queries(
        &engine,
        root,
        &QueryFilter::default(),
        &paths(&[
            "script",
            "page.html",
            "note.md",
            "settings.json",
            "source.rs",
        ]),
    )
    .await?;
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        assert_eq!(
            search_paths(&engine, root, mode, filter.clone()).await?,
            paths(&["settings.json", "source.rs"])
        );
    }
    let python = QueryFilter {
        formats: vec![FileFormat::Python],
        ..QueryFilter::default()
    };
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        assert!(
            search_paths(&engine, root, mode, python.clone())
                .await?
                .is_empty(),
            "a shebang helps extraction but does not change filename-based query filters"
        );
        assert_eq!(
            search_paths(
                &engine,
                root,
                mode,
                QueryFilter {
                    categories: vec![FileCategory::Document],
                    ..QueryFilter::default()
                }
            )
            .await?,
            paths(&["page.html", "note.md"])
        );
    }

    let info = engine.info(info_options(root)).await?;
    let files = native_file_records(&info.index_path)?;
    let script = files
        .iter()
        .find(|file| file["relative_path"]["value"] == "script")
        .expect("stored script");
    assert!(script.get("formats").is_none());

    fs::write(
        root.join("script"),
        "orchard is now a plain text document with no shebang",
    )?;
    let updated = engine.index(index_options(root)).await?;
    assert_eq!(
        (
            updated.files_modified,
            updated.files_deleted,
            updated.files_failed
        ),
        (1, 0, 0)
    );
    let files = native_file_records(&info.index_path)?;
    let script = files
        .iter()
        .find(|file| file["relative_path"]["value"] == "script")
        .expect("reclassified script stays indexed");
    assert!(script.get("formats").is_none());
    assert_index_and_queries(
        &engine,
        root,
        &QueryFilter::default(),
        &paths(&[
            "script",
            "page.html",
            "note.md",
            "settings.json",
            "source.rs",
        ]),
    )
    .await?;
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        assert_eq!(
            search_paths(&engine, root, mode, filter.clone()).await?,
            paths(&["settings.json", "source.rs"])
        );
        assert!(
            search_paths(&engine, root, mode, python.clone())
                .await?
                .is_empty()
        );
    }
    fs::remove_file(root.join("source.rs"))?;
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        assert_eq!(
            search_paths(
                &engine,
                root,
                mode,
                QueryFilter {
                    formats: vec![FileFormat::Rust],
                    ..QueryFilter::default()
                },
            )
            .await?,
            paths(&["source.rs"]),
            "format filtering uses the stored filename without reading the missing source"
        );
    }
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Compare positive catalog filtering and native NOT LIKE errors through both public retrieval routes"
)]
async fn catalog_name_predicates_filter_both_native_search_routes() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    let sources = [
        ("main.rs", "/// orchard\npub fn main() {}\n"),
        ("upper.RS", "orchard\n"),
        ("tsconfig.json", "{\"orchard\": true}\n"),
        ("CMakeLists.txt", "# orchard\nproject(orchard)\n"),
        ("notes.txt", "orchard\n"),
        ("script", "orchard\n"),
        ("header.h", "/// orchard\nint orchard(void);\n"),
    ];
    write_sources(root, &sources)?;
    let engine = ZvecGrep::new();
    let indexed = engine.index(index_options(root)).await?;
    assert_eq!(
        (indexed.files_added, indexed.files_failed),
        (sources.len(), 0)
    );
    for (name, _) in sources {
        fs::remove_file(root.join(name))?;
    }
    let cases = [
        (
            QueryFilter {
                formats: vec![FileFormat::Rust],
                ..Default::default()
            },
            vec!["main.rs"],
        ),
        (
            QueryFilter {
                formats: vec![FileFormat::Text],
                ..Default::default()
            },
            vec!["notes.txt"],
        ),
        (
            QueryFilter {
                formats: vec![FileFormat::Cmake],
                ..Default::default()
            },
            vec!["CMakeLists.txt"],
        ),
        (
            QueryFilter {
                formats: vec![FileFormat::Json],
                categories: vec![FileCategory::Code],
                ..Default::default()
            },
            vec!["tsconfig.json"],
        ),
    ];
    for (filter, expected) in cases {
        for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
            assert_eq!(
                search_paths(&engine, root, mode, filter.clone()).await?,
                paths(&expected),
                "{mode:?}: {filter:?}"
            );
        }
    }

    let info = engine.info(info_options(root)).await?;
    let dimension = info
        .workspace_index
        .expect("workspace")
        .embedding
        .expect("model")
        .dimension;
    let collections = fs::read_dir(&info.index_path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|path| {
            path.file_name()
                .expect("collection name")
                .to_string_lossy()
                .starts_with("fragments_")
        })
        .collect::<Vec<_>>();
    assert_eq!(collections.len(), 1, "one native model collection");
    let collection_path = &collections[0];
    #[cfg(windows)]
    let collection_path = dunce::simplified(collection_path);
    let mut collection_options = zvec_rust::CollectionOptions::new()?;
    collection_options.set_read_only(true)?;
    let collection = zvec_rust::Collection::open(
        collection_path.to_str().expect("UTF-8 collection path"),
        Some(&collection_options),
    )?;
    let mut fts = zvec_rust::Fts::new()?;
    fts.set_match_string("orchard")?;
    for (mode, operation, mut native_query) in [
        (
            ContextRouteMode::Fts,
            "search full-text index",
            zvec_rust::SearchQuery::fts("text", &fts, 64)?,
        ),
        (
            ContextRouteMode::Vector,
            "search vector index",
            zvec_rust::SearchQuery::new("embedding", &vec![1.0; dimension], 64)?,
        ),
    ] {
        // Excluding Rust negates both its suffix and the dot-only filename exception.
        native_query.set_filter("(file_name NOT LIKE '%.rs' OR file_name = '.rs')")?;
        native_query.set_output_fields(&["document_id", "entity_id", "file_id"])?;
        native_query.set_include_vector(false)?;
        let native_error = collection
            .query(&native_query)
            .err()
            .expect("zvec 0.7.2 does not parse NOT LIKE");
        assert!(native_error.to_string().contains("syntax error"));
        let error = engine
            .context(ContextOptions {
                root: Some(root.to_path_buf()),
                routes: vec![ContextRoute {
                    mode,
                    query: "orchard".into(),
                }],
                filter: QueryFilter {
                    excluded_formats: vec![FileFormat::Rust],
                    ..Default::default()
                },
                limit: Some(64),
                auto_update: false,
                allow_remote: true,
                ..ContextOptions::default()
            })
            .await
            .expect_err("native NOT LIKE error must not be replaced with a successful fallback");
        assert_eq!(error.code(), zg_engine::EngineError::STORAGE_FAILURE);
        assert_eq!(error.message(), format!("zvec {operation}: {native_error}"));
    }
    collection.close()?;

    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Exercise the complete persisted update and rebuild lifecycle through the public API"
)]
async fn explicit_empty_false_and_null_updates_persist_and_survive_rebuild() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    write_sources(
        root,
        &[
            ("normal.txt", "orchard normal source"),
            (".hidden.txt", "orchard hidden source"),
            ("skipped.txt", "orchard ignored source"),
            (".gitignore", "skipped.txt\n"),
            (".customignore", "nothing.matches\n"),
            ("deep/nested.md", "# orchard nested document\n"),
        ],
    )?;
    fs::write(root.join("large.txt"), "orchard large source ".repeat(20))?;
    let engine = ZvecGrep::new();
    let initial = engine
        .index(IndexOptions {
            scan: ScanRulesUpdate {
                globs: Some(vec!["*.txt".into()]),
                hidden: Some(true),
                no_ignore: Some(true),
                follow_symlinks: Some(true),
                ignore_files: Some(vec![PathBuf::from(".customignore")]),
                max_depth: Some(Some(1)),
                max_file_size_bytes: Some(Some(40)),
                ..ScanRulesUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((initial.files_added, initial.files_failed), (3, 0));
    let before = engine
        .info(info_options(root))
        .await?
        .workspace_index
        .expect("workspace");
    let requests = server.requests.load(Ordering::Acquire);
    let inputs = server.inputs.load(Ordering::Acquire);
    assert_eq!(engine.index(index_options(root)).await?.files_unchanged, 3);
    let unchanged = engine
        .info(info_options(root))
        .await?
        .workspace_index
        .expect("workspace");
    assert_eq!(unchanged.scan, before.scan);
    assert_eq!(server.requests.load(Ordering::Acquire), requests);
    assert_eq!(server.inputs.load(Ordering::Acquire), inputs);

    let scan: ScanRulesUpdate = serde_json::from_value(json!({
        "globs": [], "hidden": false, "no_ignore": false, "follow_symlinks": false,
        "ignore_files": [], "max_depth": null, "max_file_size_bytes": null
    }))?;
    let updated = engine
        .index(IndexOptions {
            scan,
            ..index_options(root)
        })
        .await?;
    assert_eq!(
        (
            updated.files_added,
            updated.files_deleted,
            updated.files_failed
        ),
        (2, 2, 0)
    );
    let expected_filter = QueryFilter::default();
    let expected_paths = paths(&["normal.txt", "large.txt", "deep/nested.md"]);
    assert_index_and_queries(&engine, root, &expected_filter, &expected_paths).await?;
    engine.close();

    let engine = ZvecGrep::new();
    let info = engine.info(info_options(root)).await?;
    let reopened = info.workspace_index.expect("reopened workspace");
    assert_eq!(reopened.scan, ScanRules::default());
    let manifest: Value = serde_json::from_slice(&fs::read(info.home.join("manifest.json"))?)?;
    assert!(manifest.get("filter").is_none());
    assert_eq!(
        manifest["scan"],
        serde_json::to_value(ScanRules::default())?
    );
    let rebuilt = engine
        .index(IndexOptions {
            rebuild: true,
            ..index_options(root)
        })
        .await?;
    assert_eq!((rebuilt.files_added, rebuilt.files_failed), (3, 0));
    let rebuilt_info = engine.info(info_options(root)).await?;
    assert_ne!(rebuilt_info.index_path, info.index_path);
    assert_eq!(
        rebuilt_info
            .workspace_index
            .expect("rebuilt workspace")
            .scan,
        ScanRules::default()
    );
    assert_index_and_queries(&engine, root, &expected_filter, &expected_paths).await?;
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

fn set_modified(path: &Path, millis: u64) -> std::io::Result<()> {
    fs::File::options().write(true).open(path)?.set_times(
        fs::FileTimes::new()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis)),
    )
}

#[tokio::test]
async fn query_filters_combine_without_changing_saved_scan_rules() -> TestResult {
    const OLD: u64 = 1_700_000_000_000;
    const INDEXED: u64 = OLD + 10_000;
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    fs::create_dir(root.join("src"))?;
    fs::write(root.join("src/old.rs"), "/// orchard\npub fn old() {}\n")?;
    fs::write(
        root.join("src/current.rs"),
        "/// orchard\npub fn current() {}\n/// orchard\npub struct Record;\n",
    )?;
    fs::write(root.join("src/readme.md"), "# orchard\n")?;
    set_modified(&root.join("src/old.rs"), OLD)?;
    set_modified(&root.join("src/current.rs"), INDEXED)?;
    set_modified(&root.join("src/readme.md"), INDEXED)?;

    let engine = ZvecGrep::new();
    engine
        .index(IndexOptions {
            scan: ScanRulesUpdate {
                globs: Some(vec!["src/**".into()]),
                ..ScanRulesUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    let before = engine.info(info_options(root)).await?.workspace_index;

    // A query must use stored timestamps even if source metadata has since changed.
    set_modified(&root.join("src/current.rs"), INDEXED + 10_000)?;
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        let result = engine
            .context(ContextOptions {
                root: Some(root.to_path_buf()),
                routes: vec![ContextRoute {
                    mode,
                    query: "orchard".into(),
                }],
                filter: QueryFilter {
                    globs: vec!["src/**".into()],
                    formats: vec![FileFormat::Rust],
                    modified_after_epoch_ms: Some(INDEXED),
                    modified_before_epoch_ms: Some(INDEXED),
                    symbol_types: vec![SymbolType::Function],
                    ..QueryFilter::default()
                },
                auto_update: false,
                allow_remote: true,
                ..ContextOptions::default()
            })
            .await?;
        assert!(
            !result.items.is_empty(),
            "{mode:?} must match the stored snapshot"
        );
        assert!(result.items.iter().all(|item| {
            item.relative_path == Path::new("src/current.rs")
                && matches!(&item.metadata, Some(EntityMetadata::Code(metadata))
                    if metadata.symbol_type == Some(SymbolType::Function))
        }));
    }
    let direct = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            rg: true,
            query: Some("orchard".into()),
            rg_paths: vec!["src/current.rs".into()],
            rg_options: RgOptions {
                modified_after_epoch_ms: Some(INDEXED),
                modified_before_epoch_ms: Some(INDEXED),
                ..RgOptions::default()
            },
            ..ContextOptions::default()
        })
        .await?;
    assert!(
        direct.items.is_empty(),
        "direct search uses the current filesystem time"
    );
    assert_eq!(
        engine.info(info_options(root)).await?.workspace_index,
        before
    );
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}
