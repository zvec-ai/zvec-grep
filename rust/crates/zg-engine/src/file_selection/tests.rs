use std::{
    fs,
    path::{Path, PathBuf},
};

use zg_host_native::{NativeScanner, PathPolicy, ScanRequest, TaskControl, WorkspaceScannerPort};

use super::{GlobMatcher, ScanPolicy};
use crate::domain::{GlobRule, ScanRules};

fn make_policy(
    root: &Path,
    globs: &[GlobRule],
    options: &ScanRules,
) -> crate::EngineResult<ScanPolicy> {
    ScanPolicy::new(
        root,
        &ScanRules {
            globs: globs.to_vec(),
            ..options.clone()
        },
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
                        &ScanRules {
                            globs: globs.to_vec(),
                            ..options.clone()
                        },
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
    let options = ScanRules {
        no_ignore: true,
        ..ScanRules::default()
    };
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
    for no_ignore in [false, true] {
        assert_eq!(
            scan(
                root,
                &globs,
                &ScanRules {
                    no_ignore,
                    hidden: true,
                    ..ScanRules::default()
                },
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
    let mut options = ScanRules {
        nested_git: false,
        no_ignore: true,
        hidden: true,
        ..ScanRules::default()
    };
    assert_eq!(
        scan(root, &globs, &options, vec![]).await,
        vec![PathBuf::from("main.rs")]
    );
    assert!(
        scan(root, &globs, &options, vec![root.join("module/code.rs")])
            .await
            .is_empty()
    );
    options.nested_git = true;
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
        &ScanRules {
            nested_git: false,
            ..ScanRules::default()
        },
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
        &ScanRules {
            nested_git: false,
            ..ScanRules::default()
        },
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
        &ScanRules {
            ignore_files: vec![external.path().to_path_buf()],
            ..ScanRules::default()
        },
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
fn external_ignore_aliases_match_canonical_change_events() {
    let temporary = tempfile::tempdir().expect("fixture");
    let base = fs::canonicalize(temporary.path()).expect("canonical fixture");
    let root = base.join("workspace");
    fs::create_dir(&root).expect("workspace");
    write(&base, "rules.ignore", "blocked/\n");
    let policy = make_policy(
        &root,
        &[],
        &ScanRules {
            no_ignore: true,
            ignore_files: vec![PathBuf::from("../rules.ignore")],
            ..ScanRules::default()
        },
    )
    .expect("policy");
    let rules = base.join("rules.ignore");
    assert!(policy.control_paths().contains(&rules));
    assert!(!policy.can_descend(&root.join("blocked")).expect("excluded"));
    fs::remove_file(&rules).expect("delete rules");
    assert_eq!(
        policy
            .control_file_changed(&rules)
            .expect("canonical event"),
        Some(PathBuf::new())
    );
    assert!(policy.can_descend(&root.join("blocked")).expect("included"));
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
        &ScanRules {
            ignore_files: vec![alias.clone()],
            ..ScanRules::default()
        },
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

#[tokio::test]
async fn ripgrep_type_names_filter_scans_and_explicit_globs() {
    let temporary = tempfile::tempdir().expect("workspace");
    let root = temporary.path();
    for path in ["src/a.h", "src/b.hpp", "src/c.cpp", "src/d.ts", "src/e.py"] {
        write(root, path, "fixture");
    }
    let rules = ScanRules {
        file_types: vec!["h".into()],
        ..ScanRules::default()
    };
    let result = scan(root, &[], &rules, vec![]).await;
    assert!(result.contains(&PathBuf::from("src/a.h")));
    assert!(result.contains(&PathBuf::from("src/b.hpp")));
    assert!(!result.contains(&PathBuf::from("src/c.cpp")));
    let excluded = ScanRules {
        excluded_file_types: vec!["h".into()],
        ..rules
    };
    assert!(
        scan(root, &["src/**".into()], &excluded, vec![])
            .await
            .is_empty()
    );
    assert!(
        ScanPolicy::new(
            root,
            &ScanRules {
                file_types: vec!["not-a-real-type".into()],
                ..ScanRules::default()
            }
        )
        .is_err()
    );
}
