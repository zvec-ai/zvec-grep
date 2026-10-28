use std::{
    fs,
    path::{Path, PathBuf},
};

use zg_host_native::{NativeScanner, PathPolicy, ScanRequest, TaskControl, WorkspaceScannerPort};

use super::{GlobMatcher, ScanPolicy};
use crate::{
    api::index::options::ScanRulesUpdate,
    domain::{FileFormat, GlobRule, ScanRules},
};

fn make_policy(
    root: &Path,
    globs: &[GlobRule],
    options: &ScanRules,
) -> crate::EngineResult<ScanPolicy> {
    ScanPolicy::new(
        root,
        &ScanRulesUpdate {
            globs: Some(globs.to_vec()),
            ..ScanRulesUpdate::default()
        }
        .apply(options)?,
    )
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().expect("parent")).expect("directories");
    fs::write(path, text).expect("fixture file");
}

async fn scan(
    root: &Path,
    globs: &[GlobRule],
    options: &ScanRules,
    scope: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let snapshot = NativeScanner::new()
        .discover(
            &ScanRequest {
                roots: vec![
                    ScanPolicy::root_spec(
                        root,
                        &ScanRulesUpdate {
                            globs: Some(globs.to_vec()),
                            ..ScanRulesUpdate::default()
                        }
                        .apply(options)
                        .expect("scan rules"),
                    )
                    .expect("policy"),
                ],
                scope_paths: scope,
            },
            &TaskControl::default(),
        )
        .await
        .expect("scan");
    let mut paths: Vec<_> = snapshot
        .files
        .into_iter()
        .map(|file| file.relative_path)
        .collect();
    paths.sort();
    paths
}

#[tokio::test]
async fn filename_formats_intersect_globs_and_preserve_scan_boundaries() {
    let temporary = tempfile::tempdir().expect("workspace");
    let root = temporary.path();
    for path in [
        "src/main.rs",
        "src/other.py",
        "src/header.h",
        "src/header.hpp",
        "src/ignored.rs",
        "src/.hidden.rs",
        "outside.rs",
        ".hidden/directory.rs",
        "nested.py/main.rs",
    ] {
        write(root, path, "example");
    }
    write(root, ".gitignore", "src/ignored.rs\n");
    let rules = ScanRulesUpdate {
        included_formats: Some(vec![FileFormat::Rust, FileFormat::Cpp]),
        excluded_formats: Some(vec![FileFormat::C]),
        ..Default::default()
    }
    .apply(&ScanRules::default())
    .expect("format rules");
    assert_eq!(
        scan(root, &[], &rules, vec![]).await,
        [
            "nested.py/main.rs",
            "outside.rs",
            "src/header.hpp",
            "src/main.rs"
        ]
        .map(PathBuf::from)
    );
    let globs = vec![GlobRule::from("src/**")];
    let expected = [
        "src/.hidden.rs",
        "src/header.hpp",
        "src/ignored.rs",
        "src/main.rs",
    ]
    .map(PathBuf::from);
    assert_eq!(scan(root, &globs, &rules, vec![]).await, expected);
    assert_eq!(
        scan(root, &globs, &rules, vec![root.join("src")]).await,
        expected
    );
    let policy = make_policy(root, &globs, &rules).expect("watch policy");
    assert!(policy.can_descend(&root.join("src")).expect("directory"));
    assert!(
        policy
            .includes_file(&root.join("src/new.rs"))
            .expect("Rust file")
    );
    assert!(
        !policy
            .includes_file(&root.join("src/new.py"))
            .expect("Python file")
    );
    assert!(
        !policy
            .includes_file(&root.join("src/new.h"))
            .expect("ambiguous header")
    );
}

#[tokio::test]
async fn scan_and_query_use_identical_ordered_path_rules() {
    let temporary = tempfile::tempdir().expect("workspace");
    let root = temporary.path();
    let paths = [
        "main.rs",
        "other.txt",
        "blocked/keep.rs",
        "src/UPPER.RS",
        "src/lower.rs",
    ];
    for path in paths {
        write(root, path, "example");
    }
    let options = ScanRulesUpdate {
        use_default_ignores: Some(false),
        ..ScanRulesUpdate::default()
    }
    .apply(&ScanRules::default())
    .expect("scan rules");
    let cases = [
        vec!["*.rs".into()],
        vec!["!blocked".into(), "blocked/keep.rs".into()],
        vec!["src/**".into(), "!**/lower.rs".into()],
        vec![
            GlobRule {
                pattern: "*.RS".into(),
                case_insensitive: true,
            },
            "!src/lower.rs".into(),
        ],
        vec![
            "!src/lower.rs".into(),
            GlobRule {
                pattern: "*.RS".into(),
                case_insensitive: true,
            },
        ],
    ];
    for globs in cases {
        let matcher = GlobMatcher::new(root, &globs).expect("matcher");
        let mut expected: Vec<_> = paths
            .iter()
            .map(PathBuf::from)
            .filter(|path| matcher.matches_path(path))
            .collect();
        expected.sort();
        assert_eq!(
            scan(root, &globs, &options, vec![]).await,
            expected,
            "{globs:?}"
        );
    }
}

#[tokio::test]
async fn nested_repositories_are_selected_and_metadata_is_never_scanned() {
    let temporary = tempfile::tempdir().expect("workspace");
    let root = temporary.path();
    write(root, "nested/.git/config", "git metadata");
    write(root, "nested/code.rs", "fn main() {}");
    write(root, "nested/.zvec-grep/state.rs", "index metadata");
    let globs = vec!["nested/**".into()];
    for use_default_ignores in [false, true] {
        assert_eq!(
            scan(
                root,
                &globs,
                &ScanRulesUpdate {
                    use_default_ignores: Some(use_default_ignores),
                    include_hidden: Some(true),
                    ..ScanRulesUpdate::default()
                }
                .apply(&ScanRules::default())
                .expect("scan rules"),
                vec![]
            )
            .await,
            vec![PathBuf::from("nested/code.rs")]
        );
    }
}

#[tokio::test]
async fn explicit_globs_override_ignores_but_scope_cannot_bypass_excluded_parents() {
    let temporary = tempfile::tempdir().expect("workspace");
    let root = temporary.path();
    write(root, ".gitignore", "nested/\n");
    write(root, "nested/.git/config", "metadata");
    write(root, "nested/code.rs", "fn main() {}");
    assert!(
        scan(root, &[], &ScanRules::default(), vec![])
            .await
            .is_empty()
    );
    let globs = vec!["nested".into(), "nested/**".into()];
    assert_eq!(
        scan(root, &globs, &ScanRules::default(), vec![]).await,
        vec![PathBuf::from("nested/code.rs")]
    );
    let globs = vec!["!nested".into(), "nested/code.rs".into()];
    assert!(
        scan(
            root,
            &globs,
            &ScanRules::default(),
            vec![root.join("nested/code.rs")]
        )
        .await
        .is_empty()
    );
}

#[tokio::test]
async fn nested_git_switch_prunes_directory_and_file_markers_including_incremental_scopes() {
    let temporary = tempfile::tempdir().expect("workspace");
    let root = temporary.path();
    write(root, ".git/config", "workspace metadata");
    write(root, "main.rs", "root source");
    write(root, "nested/.git/config", "nested metadata");
    write(root, "nested/code.rs", "nested source");
    write(root, "module/.git", "gitdir: ../.git/modules/module\n");
    write(root, "module/code.rs", "submodule source");
    let globs = vec!["**".into()];
    let mut options = ScanRulesUpdate {
        nested_git: Some(false),
        use_default_ignores: Some(false),
        include_hidden: Some(true),
        ..ScanRulesUpdate::default()
    }
    .apply(&ScanRules::default())
    .expect("scan rules");
    assert_eq!(
        scan(root, &globs, &options, vec![]).await,
        vec![PathBuf::from("main.rs")]
    );
    assert!(
        scan(root, &globs, &options, vec![root.join("module/code.rs")])
            .await
            .is_empty()
    );
    options = ScanRulesUpdate {
        nested_git: Some(true),
        ..ScanRulesUpdate::default()
    }
    .apply(&options)
    .expect("updated scan rules");
    assert_eq!(
        scan(root, &globs, &options, vec![]).await,
        vec![
            PathBuf::from("main.rs"),
            PathBuf::from("module/code.rs"),
            PathBuf::from("nested/code.rs")
        ]
    );
}

#[test]
fn repository_marker_changes_reconcile_membership_without_affecting_workspace_root() {
    let temporary = tempfile::tempdir().expect("workspace");
    let root = temporary.path();
    write(root, ".git/config", "workspace metadata");
    write(root, "module/code.rs", "source");
    let policy = make_policy(
        root,
        &[],
        &ScanRulesUpdate {
            nested_git: Some(false),
            ..ScanRulesUpdate::default()
        }
        .apply(&ScanRules::default())
        .expect("scan rules"),
    )
    .expect("policy");
    assert!(policy.can_descend(root).expect("root remains included"));
    assert_eq!(
        policy
            .control_file_changed(&root.join(".git"))
            .expect("root metadata"),
        None
    );
    assert!(
        policy
            .can_descend(&root.join("module"))
            .expect("ordinary directory")
    );
    write(root, "module/.git", "gitdir: ../.git/modules/module\n");
    let marker = root.join("module/.git");
    assert_eq!(
        policy
            .control_file_changed(&marker)
            .expect("marker created"),
        Some(PathBuf::from("module"))
    );
    assert!(
        !policy
            .can_descend(&root.join("module"))
            .expect("submodule pruned")
    );
    assert!(policy.control_paths().contains(&marker));
    fs::remove_file(&marker).expect("marker removed");
    assert_eq!(
        policy
            .control_file_changed(&marker)
            .expect("marker deleted"),
        Some(PathBuf::from("module"))
    );
    assert!(
        policy
            .can_descend(&root.join("module"))
            .expect("ordinary directory restored")
    );
    write(
        root,
        "module/.git/config",
        "now a regular nested repository",
    );
    policy.invalidate().expect("recover missed event");
    assert!(
        !policy
            .can_descend(&root.join("module"))
            .expect("directory marker pruned")
    );
    let excluded = make_policy(
        root,
        &["!module".into()],
        &ScanRulesUpdate {
            nested_git: Some(false),
            ..ScanRulesUpdate::default()
        }
        .apply(&ScanRules::default())
        .expect("scan rules"),
    )
    .expect("excluded policy");
    assert!(
        !excluded
            .can_descend(&root.join("module"))
            .expect("glob exclusion")
    );
    assert!(!excluded.control_paths().contains(&marker));
}

#[test]
fn ignore_control_changes_invalidate_rules_even_when_not_selected() {
    let temporary = tempfile::tempdir().expect("workspace");
    let root = temporary.path();
    write(root, ".gitignore", "blocked/\n");
    let policy = make_policy(root, &[], &ScanRules::default()).expect("policy");
    assert!(!policy.can_descend(&root.join("blocked")).expect("excluded"));
    write(root, ".gitignore", "");
    assert_eq!(
        policy
            .control_file_changed(&root.join(".gitignore"))
            .expect("changed"),
        Some(PathBuf::new())
    );
    assert!(policy.can_descend(&root.join("blocked")).expect("included"));
    let external = tempfile::NamedTempFile::new().expect("external rules");
    fs::write(external.path(), "blocked/\n").expect("rules");
    let policy = make_policy(
        root,
        &[],
        &ScanRulesUpdate {
            ignore_files: Some(vec![external.path().to_path_buf()]),
            ..ScanRulesUpdate::default()
        }
        .apply(&ScanRules::default())
        .expect("scan rules"),
    )
    .expect("policy");
    assert!(!policy.can_descend(&root.join("blocked")).expect("excluded"));
    assert!(
        policy
            .control_paths()
            .contains(&fs::canonicalize(external.path()).expect("canonical rules"))
    );
    fs::write(external.path(), "").expect("rules");
    assert_eq!(
        policy
            .control_file_changed(external.path())
            .expect("changed"),
        Some(PathBuf::new())
    );
    assert!(policy.can_descend(&root.join("blocked")).expect("included"));
}

#[test]
fn saved_ignore_files_preserve_internal_external_precedence() {
    let temporary = tempfile::tempdir().expect("fixture");
    let base = fs::canonicalize(temporary.path()).expect("canonical fixture");
    let root = base.join("workspace");
    for path in ["one.rs", "two.rs", "three.rs"] {
        write(&root, path, "source");
    }
    write(&root, "first.ignore", "one.rs\ntwo.rs\nthree.rs\n");
    write(&base, "shared.ignore", "!one.rs\n!two.rs\n");
    write(&root, "last.ignore", "one.rs\n");
    let mut ignore_files = vec![
        PathBuf::from("first.ignore"),
        base.join("shared.ignore"),
        PathBuf::from("last.ignore"),
    ];

    for includes_one in [false, true] {
        let rules = ScanRulesUpdate {
            use_default_ignores: Some(false),
            ignore_files: Some(ignore_files.clone()),
            ..ScanRulesUpdate::default()
        }
        .apply(&ScanRules::default())
        .expect("scan rules");
        let saved = serde_json::to_vec(&rules).expect("save scan rules");
        let restored: ScanRules = serde_json::from_slice(&saved).expect("restore scan rules");
        assert_eq!(
            restored.ignore_files().collect::<Vec<_>>(),
            ignore_files
                .iter()
                .map(PathBuf::as_path)
                .collect::<Vec<_>>()
        );
        let policy = make_policy(&root, &[], &restored).expect("restored policy");
        assert_eq!(
            policy.includes_file(&root.join("one.rs")).expect("one.rs"),
            includes_one,
            "the last matching ignore file must win: {ignore_files:?}"
        );
        assert!(policy.includes_file(&root.join("two.rs")).expect("two.rs"));
        assert!(
            !policy
                .includes_file(&root.join("three.rs"))
                .expect("three.rs")
        );
        ignore_files.swap(1, 2);
    }
}

#[test]
fn adversarial_globs_are_bounded_and_do_not_use_backtracking_regex() {
    let globs = vec![format!("{}Z", "**".repeat(20)).into()];
    let matcher = GlobMatcher::new(Path::new("/workspace"), &globs).expect("bounded glob");
    assert!(!matcher.matches_path(Path::new("src/authorization/operation.ts")));
    for globs in [
        vec!["x".repeat(4_097).into()],
        vec![GlobRule::from("*.rs"); 1_025],
    ] {
        assert!(GlobMatcher::new(Path::new("/workspace"), &globs).is_err());
    }
}

#[test]
fn reconciliation_refreshes_ignore_rules_after_a_missed_deletion_event() {
    let temporary = tempfile::tempdir().expect("workspace");
    let root = temporary.path();
    write(root, "src/.gitignore", "blocked/\n");
    let policy = make_policy(root, &[], &ScanRules::default()).expect("policy");
    assert!(
        !policy
            .can_descend(&root.join("src/blocked"))
            .expect("excluded")
    );
    fs::remove_file(root.join("src/.gitignore")).expect("delete ignore file");
    policy
        .invalidate()
        .expect("refresh policy after missed event");
    assert!(
        policy
            .can_descend(&root.join("src/blocked"))
            .expect("included")
    );
    write(root, "src/.gitignore", "blocked/\n");
    policy
        .control_file_changed(&root.join("src/.gitignore"))
        .expect("recreated ignore file");
    assert!(
        !policy
            .can_descend(&root.join("src/blocked"))
            .expect("excluded again")
    );
}

#[test]
fn large_ignore_files_fail_explicitly_instead_of_unbounded_compilation() {
    let temporary = tempfile::tempdir().expect("workspace");
    let root = temporary.path();
    write(root, ".gitignore", &"*".repeat(1_048_577));
    let policy = make_policy(root, &[], &ScanRules::default()).expect("policy");
    let error = policy
        .includes_file(&root.join("main.rs"))
        .expect_err("oversized rules");
    assert!(error.to_string().contains("limit"));
}

#[test]
fn missing_explicit_ignore_files_are_skipped_and_reloaded() {
    let temporary = tempfile::tempdir().expect("fixture");
    let base = fs::canonicalize(temporary.path()).expect("canonical fixture");
    let root = base.join("source");
    fs::create_dir(&root).expect("empty source root");
    for path in [
        PathBuf::from("local.ignore"),
        PathBuf::from("missing/nested/local.ignore"),
        base.join("external.ignore"),
        base.join("missing/nested/external.ignore"),
    ] {
        let absolute = root.join(&path);
        let rules = ScanRulesUpdate {
            ignore_files: Some(vec![path.clone()]),
            ..ScanRulesUpdate::default()
        }
        .apply(&ScanRules::default())
        .expect("scan rules");
        let policy = make_policy(&root, &[], &rules).expect("missing rules are skipped");
        assert_eq!(rules.ignore_files().collect::<Vec<_>>(), [path.as_path()]);
        assert!(policy.control_paths().contains(&absolute));
        assert!(
            policy
                .includes_file(&root.join("main.rs"))
                .expect("included")
        );
        fs::create_dir_all(absolute.parent().expect("ignore parent")).expect("ignore directories");
        fs::write(&absolute, "main.rs\n").expect("create rules");
        assert_eq!(
            policy
                .control_file_changed(&absolute)
                .expect("created rules"),
            Some(PathBuf::new())
        );
        assert!(
            !policy
                .includes_file(&root.join("main.rs"))
                .expect("excluded")
        );
        fs::remove_file(&absolute).expect("delete rules");
        policy
            .control_file_changed(&absolute)
            .expect("deleted rules");
        assert!(
            policy
                .includes_file(&root.join("main.rs"))
                .expect("included")
        );
        assert!(policy.control_paths().contains(&absolute));
        fs::write(&absolute, "main.rs\n").expect("restore rules");
        policy
            .control_file_changed(&absolute)
            .expect("restored rules");
        assert!(
            !policy
                .includes_file(&root.join("main.rs"))
                .expect("excluded")
        );
    }
}

#[test]
fn invalid_explicit_ignore_files_are_rejected_even_with_whitelist_globs() {
    let temporary = tempfile::tempdir().expect("fixture");
    let root = temporary.path();
    let path = root.join("invalid.ignore");
    let rules = ScanRulesUpdate {
        ignore_files: Some(vec![path.clone()]),
        ..ScanRulesUpdate::default()
    }
    .apply(&ScanRules::default())
    .expect("scan rules");
    for (contents, message) in [
        (&b"\xff"[..], "could not read ignore file"),
        (&b"[z-a]\n"[..], "invalid ignore rule"),
    ] {
        fs::write(&path, contents).expect("invalid rules");
        for globs in [vec![], vec!["*.rs".into()]] {
            let error = make_policy(root, &globs, &rules).expect_err("invalid rules");
            assert!(error.message().contains(message));
            assert!(error.message().contains("invalid.ignore"));
        }
    }
}

#[test]
fn explicit_ignore_files_below_regular_files_are_rejected() {
    let temporary = tempfile::tempdir().expect("workspace");
    let root = temporary.path();
    write(root, "source.rs", "source");
    for path in [
        "source.rs/required.ignore",
        "source.rs/missing/nested/required.ignore",
    ] {
        let rules = ScanRulesUpdate {
            ignore_files: Some(vec![root.join(path)]),
            ..ScanRulesUpdate::default()
        }
        .apply(&ScanRules::default())
        .expect("scan rules");
        let error = make_policy(root, &[], &rules)
            .expect_err("an explicit ignore file must not be treated as absent directory rules");
        assert!(error.message().contains("required.ignore"));
    }
}

#[test]
fn external_ignore_aliases_match_canonical_change_events() {
    let temporary = tempfile::tempdir().expect("fixture");
    let base = fs::canonicalize(temporary.path()).expect("canonical fixture");
    let root = base.join("workspace");
    fs::create_dir(&root).expect("workspace");
    write(&base, "rules.ignore", "blocked/\n");
    let mut update = ScanRulesUpdate {
        use_default_ignores: Some(false),
        ignore_files: Some(vec![PathBuf::from("../rules.ignore")]),
        ..ScanRulesUpdate::default()
    };
    update
        .normalize_ignore_paths(&root)
        .expect("normalize external ignore path");
    let policy = make_policy(
        &root,
        &[],
        &update.apply(&ScanRules::default()).expect("scan rules"),
    )
    .expect("policy");
    let rules = base.join("rules.ignore");
    assert!(policy.control_paths().contains(&rules));
    assert!(!policy.can_descend(&root.join("blocked")).expect("excluded"));
    fs::remove_file(&rules).expect("delete rules");
    assert_eq!(
        policy.control_file_changed(&rules).expect("deleted rules"),
        Some(PathBuf::new())
    );
    assert!(policy.can_descend(&root.join("blocked")).expect("included"));
    assert!(policy.control_paths().contains(&rules));
    fs::write(&rules, "blocked/\n").expect("restore explicit ignore file");
    policy
        .control_file_changed(&rules)
        .expect("restored rules event");
    assert!(!policy.can_descend(&root.join("blocked")).expect("excluded"));
}

#[cfg(unix)]
#[test]
fn ignore_symlinks_watch_alias_and_target_and_refresh_after_retargeting() {
    let temporary = tempfile::tempdir().expect("fixture");
    let base = fs::canonicalize(temporary.path()).expect("canonical fixture");
    let root = base.join("workspace");
    fs::create_dir(&root).expect("workspace");
    write(&base, "first/rules", "blocked/\n");
    write(&base, "second/rules", "");
    let alias = root.join("custom.ignore");
    let first = base.join("first/rules");
    let second = base.join("second/rules");
    std::os::unix::fs::symlink(&first, &alias).expect("link rules");
    let policy = make_policy(
        &root,
        &[],
        &ScanRulesUpdate {
            ignore_files: Some(vec![alias.clone()]),
            ..ScanRulesUpdate::default()
        }
        .apply(&ScanRules::default())
        .expect("scan rules"),
    )
    .expect("policy");
    assert!(policy.control_paths().contains(&alias));
    assert!(policy.control_paths().contains(&first));
    assert!(!policy.can_descend(&root.join("blocked")).expect("excluded"));
    fs::write(&first, "").expect("edit target");
    assert_eq!(
        policy.control_file_changed(&first).expect("target event"),
        Some(PathBuf::new())
    );
    assert!(policy.can_descend(&root.join("blocked")).expect("included"));
    fs::remove_file(&alias).expect("unlink rules");
    std::os::unix::fs::symlink(&second, &alias).expect("retarget rules");
    policy.control_file_changed(&alias).expect("retarget event");
    assert!(policy.control_paths().contains(&second));
    assert!(!policy.control_paths().contains(&first));
}

#[cfg(unix)]
#[test]
fn nested_ignore_symlinks_are_controls_without_following_source_symlinks() {
    let temporary = tempfile::tempdir().expect("fixture");
    let base = fs::canonicalize(temporary.path()).expect("canonical fixture");
    let root = base.join("workspace");
    fs::create_dir_all(root.join("src")).expect("workspace");
    write(&base, "rules", "blocked/\n");
    let alias = root.join("src/.gitignore");
    let target = base.join("rules");
    std::os::unix::fs::symlink(&target, &alias).expect("link rules");
    let policy = make_policy(&root, &[], &ScanRules::default()).expect("policy");
    assert!(
        policy
            .can_descend(&root.join("src"))
            .expect("scan directory")
    );
    assert!(policy.control_paths().contains(&target));
    assert!(
        !policy
            .can_descend(&root.join("src/blocked"))
            .expect("excluded")
    );
    fs::write(&target, "").expect("edit target");
    assert_eq!(
        policy.control_file_changed(&target).expect("target event"),
        Some(PathBuf::new())
    );
    assert!(
        policy
            .can_descend(&root.join("src/blocked"))
            .expect("included")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn format_filtered_file_symlinks_do_not_fail_directory_policy_checks() {
    let temporary = tempfile::tempdir().expect("workspace");
    let root = temporary.path().join("workspace");
    let linked = temporary.path().join("linked");
    write(&root, "source.rs", "source");
    write(&linked, "child.rs", "linked source");
    write(&linked, "ignored.rs", "ignored source");
    write(&linked, ".gitignore", "ignored.rs\n");
    write(&linked, "nested/.git/config", "nested repository");
    write(&linked, "nested/hidden.rs", "nested source");
    std::os::unix::fs::symlink("source.rs", root.join("alias.txt")).expect("excluded file alias");
    std::os::unix::fs::symlink("source.rs", root.join("included.rs")).expect("included file alias");
    std::os::unix::fs::symlink(&linked, root.join("linked.txt")).expect("directory alias");
    let rules = ScanRulesUpdate {
        included_formats: Some(vec![FileFormat::Rust]),
        follow_symlinks: Some(true),
        nested_git: Some(false),
        ..ScanRulesUpdate::default()
    }
    .apply(&ScanRules::default())
    .expect("scan rules");
    assert_eq!(
        scan(&root, &[], &rules, vec![]).await,
        ["included.rs", "linked.txt/child.rs", "source.rs"].map(PathBuf::from)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn fixed_index_and_its_symlink_aliases_are_never_source_files() {
    let temporary = tempfile::tempdir().expect("workspace");
    let root = temporary.path();
    write(root, "source.txt", "source");
    write(root, ".zvec-grep/index/stored.txt", "index data");
    write(root, ".zvec-grep/temp/new/staged.txt", "staged data");
    std::os::unix::fs::symlink(root.join(".zvec-grep"), root.join("data-alias"))
        .expect("index alias");
    std::os::unix::fs::symlink(
        root.join(".zvec-grep/index/stored.txt"),
        root.join("file-alias.txt"),
    )
    .expect("index file alias");
    let rules = ScanRulesUpdate {
        include_hidden: Some(true),
        follow_symlinks: Some(true),
        use_default_ignores: Some(false),
        ..ScanRulesUpdate::default()
    }
    .apply(&ScanRules::default())
    .expect("scan rules");
    assert_eq!(
        scan(root, &[], &rules, vec![]).await,
        [PathBuf::from("source.txt")]
    );
    assert!(
        scan(root, &[], &rules, vec![root.join("data-alias")])
            .await
            .is_empty()
    );
}
