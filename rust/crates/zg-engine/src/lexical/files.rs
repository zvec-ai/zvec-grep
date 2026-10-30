use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use zg_host_native::PathPolicy;

use crate::{
    EngineError, EngineResult,
    api::{
        context::options::RgOptions,
        files::{FilesOptions, FilesResult},
    },
};

use super::{
    CheckedPaths, build_walker, check_cancelled, is_hard_ignored_path, matches_modified_time,
    run_blocking,
    types::{LexicalOptions, LexicalSearchRequest},
};

pub(crate) async fn list(options: FilesOptions) -> EngineResult<FilesResult> {
    list_with_selection(options, None).await
}

pub(crate) async fn list_with_policy(
    options: FilesOptions,
    policy: Arc<dyn PathPolicy>,
) -> EngineResult<FilesResult> {
    list_with_selection(options, Some(policy)).await
}

async fn list_with_selection(
    options: FilesOptions,
    policy: Option<Arc<dyn PathPolicy>>,
) -> EngineResult<FilesResult> {
    let root = std::path::absolute(options.root.as_deref().unwrap_or_else(|| Path::new(".")))
        .map_err(|error| EngineError::from_io("failed to resolve file discovery root", &error))?;
    let signal = options.signal.as_ref().map_or_else(
        tokio_util::sync::CancellationToken::new,
        tokio_util::sync::CancellationToken::child_token,
    );
    let _cancel_on_drop = signal.clone().drop_guard();
    let request = LexicalSearchRequest {
        signal: Some(signal),
        paths: options.paths,
        options: LexicalOptions {
            matching: RgOptions {
                glob_rules: options.globs,
                glob_case_insensitive: options.glob_case_insensitive,
                no_ignore_dot: options.no_ignore_dot,
                no_ignore_files: options.no_ignore_files,
                no_ignore_global: options.no_ignore_global,
                no_ignore_parent: options.no_ignore_parent,
                no_ignore_vcs: options.no_ignore_vcs,
                one_file_system: options.one_file_system,
                ..RgOptions::default()
            },
            hidden: options.hidden,
            no_ignore: options.no_ignore,
            follow: options.follow,
            file_types: options.file_types,
            excluded_file_types: options.excluded_file_types,
            ignore_files: options.ignore_files,
            max_depth: options.max_depth,
            max_file_size_bytes: options.max_file_size_bytes,
            modified_after_epoch_ms: options.modified_after_epoch_ms,
            modified_before_epoch_ms: options.modified_before_epoch_ms,
            ..LexicalOptions::default()
        },
        ..LexicalSearchRequest::default()
    };
    check_cancelled(&request)?;
    run_blocking(move || list_sync(&root, request, policy.as_ref())).await
}

fn list_sync(
    root: &Path,
    mut request: LexicalSearchRequest,
    policy: Option<&Arc<dyn PathPolicy>>,
) -> EngineResult<FilesResult> {
    let implicit_root = request.paths.is_empty();
    let inputs = if implicit_root {
        vec![PathBuf::from(".")]
    } else {
        std::mem::take(&mut request.paths)
    };
    let mut result = FilesResult::default();
    let policy_errors = Arc::new(Mutex::new(Vec::new()));
    for input in inputs {
        check_cancelled(&request)?;
        if input.as_os_str().is_empty() {
            result
                .errors
                .push("file discovery input path must not be empty".into());
            continue;
        }
        if input == Path::new("-") {
            result.paths.push(PathBuf::from("<stdin>"));
            continue;
        }
        request.paths = vec![input.clone()];
        // Let the walker report missing or inaccessible inputs rather than skipping them.
        let checked = CheckedPaths {
            existing: request.paths.clone(),
            missing: Vec::new(),
        };
        let mut walker = build_walker(root, &request, &checked, 1)?;
        if let Some(policy) = policy {
            apply_policy(
                &mut walker,
                root,
                Arc::clone(policy),
                Arc::clone(&policy_errors),
            );
        }
        for entry in walker.build() {
            check_cancelled(&request)?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    result.errors.push(error.to_string());
                    continue;
                }
            };
            if let Some(error) = entry.error() {
                result.errors.push(error.to_string());
            }
            let file = entry.file_type().is_some_and(|kind| kind.is_file())
                || (entry.depth() == 0 && entry.path().is_file());
            if !file
                || request.options.max_file_size_bytes == Some(0)
                || !matches_modified_time(entry.path(), &request)
            {
                continue;
            }
            let path = entry.into_path();
            result.paths.push(if input.is_absolute() {
                path
            } else if implicit_root {
                path.strip_prefix(root).unwrap_or(&path).to_path_buf()
            } else {
                let base = root.join(&input);
                let suffix = path.strip_prefix(&base).unwrap_or(&path);
                if suffix.as_os_str().is_empty() {
                    input.clone()
                } else {
                    input.join(suffix)
                }
            });
        }
    }
    check_cancelled(&request)?;
    result.errors.extend(std::mem::take(
        &mut *policy_errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    ));
    result.paths.sort();
    Ok(result)
}

fn apply_policy(
    walker: &mut ignore::WalkBuilder,
    root: &Path,
    policy: Arc<dyn PathPolicy>,
    errors: Arc<Mutex<Vec<String>>>,
) {
    let root = root.to_path_buf();
    walker.filter_entry(move |entry| {
        let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
        if is_hard_ignored_path(relative) {
            return false;
        }
        let selected = if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            policy.can_descend(entry.path())
        } else {
            policy.includes_file(entry.path())
        };
        selected.unwrap_or_else(|error| {
            errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(error.to_string());
            false
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn zero_size_limit_excludes_empty_and_explicit_files() {
        let root = tempfile::tempdir().expect("workspace");
        std::fs::write(root.path().join("empty.txt"), "").expect("empty file");
        std::fs::write(root.path().join("source.txt"), "needle\n").expect("source file");
        let policy = Arc::new(
            crate::file_selection::ScanPolicy::for_current_source(
                root.path(),
                &crate::domain::ScanRules::default(),
            )
            .expect("scan policy"),
        );
        for paths in [
            Vec::new(),
            vec![PathBuf::from("empty.txt"), PathBuf::from("source.txt")],
        ] {
            let options = FilesOptions {
                root: Some(root.path().to_path_buf()),
                paths,
                max_file_size_bytes: Some(0),
                ..FilesOptions::default()
            };
            let result = list(options.clone()).await.expect("file listing");
            assert!(result.paths.is_empty());
            assert!(result.errors.is_empty());
            let result = list_with_policy(options, policy.clone())
                .await
                .expect("file listing with policy");
            assert!(result.paths.is_empty());
            assert!(result.errors.is_empty());
        }
    }

    #[tokio::test]
    async fn cancelled_discovery_returns_before_traversal() {
        let signal = tokio_util::sync::CancellationToken::new();
        signal.cancel();
        let error = list(FilesOptions {
            signal: Some(signal),
            ..FilesOptions::default()
        })
        .await
        .expect_err("cancelled discovery");
        assert_eq!(error.code(), EngineError::CANCELLED);
    }
}
