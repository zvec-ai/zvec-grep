//! Explicit sample outcomes rendered by the production CLI presentation API.
//!
//! No operation is executed. Error origins deliberately point to this fixture
//! file; OS/provider-specific details below are labelled samples, not captured
//! failures. The color-disabled cards use `--no-color`'s renderer policy, which
//! has the same appearance as automatic color with `NO_COLOR` set.

use std::{io, path::Path};

use zg_cli::{ColorMode, MessageKind, OutputOptions};
use zg_engine::{
    EngineError,
    api::{
        index::IndexResult,
        info::{
            InfoResult,
            result::{
                FailedFile, IndexCompatibility, IndexStatus, InfoSource, WorkspaceIndexPolicy,
            },
        },
    },
    authorization::{AuthorizationGrantStatus, AuthorizationStatus},
};

use super::{Fixture, info_fixture, options, render};

pub(super) fn fixtures(root: &Path) -> io::Result<Vec<Fixture>> {
    let mut fixtures = core_errors(root)?;
    fixtures.extend(remote_and_storage_errors(root)?);
    fixtures.extend(error_presentations(root)?);
    fixtures.extend(index_outcomes(root)?);
    fixtures.extend(status_outcomes(root)?);
    fixtures.extend(authorization_outcomes(root)?);
    fixtures.extend(configuration_outcomes(root)?);
    Ok(fixtures)
}

fn error_card(
    id: &'static str,
    title: &'static str,
    command: &'static str,
    error: EngineError,
    output_options: OutputOptions,
    terminal: bool,
) -> io::Result<Fixture> {
    let retryable = error.is_retryable();
    let report = error.into_report();
    render(id, title, command, |output| {
        zg_cli::write_error_report(output, &report, Some(retryable), output_options, terminal)
    })
}

fn core_errors(root: &Path) -> io::Result<Vec<Fixture>> {
    // These messages mirror constructors in authorization.rs, service.rs,
    // workspace/lock.rs, and pipelines/indexed_search/service.rs.
    let cases = [
        (
            "error-invalid-argument",
            "Invalid argument · fixture",
            "zg --index --endpoint invalid",
            EngineError::invalid_argument("Invalid remote embedding endpoint"),
        ),
        (
            "error-not-found",
            "Index unavailable · fixture",
            "zg --hybrid search --mode direct",
            EngineError::not_found(format!(
                "workspace index at {}: workspace index has not been built",
                root.display()
            )),
        ),
        (
            "error-unsupported",
            "Index disabled · fixture",
            "zg --hybrid search",
            EngineError::unsupported(format!(
                "workspace indexing is disabled at {}",
                root.display()
            )),
        ),
        (
            "error-resource-busy",
            "Workspace busy · fixture",
            "zg --index .",
            EngineError::resource_busy(format!(
                "lock is busy: lock={} operation=index",
                root.join(".zvec-grep/lock").display()
            )),
        ),
        (
            "error-resource-closed",
            "Closed engine · error-report fixture",
            "EngineError::resource_closed fixture",
            EngineError::resource_closed("engine instance has been closed"),
        ),
        (
            "error-internal",
            "Internal failure · error-report fixture",
            "EngineError::internal fixture",
            EngineError::internal("embedded grep result collector was poisoned"),
        ),
    ];
    cases
        .into_iter()
        .map(|(id, title, command, error)| error_card(id, title, command, error, options(), true))
        .collect()
}

fn remote_and_storage_errors(root: &Path) -> io::Result<Vec<Fixture>> {
    // Sample OS/provider causes use the real engine error-code mapping. They do
    // not claim that this process performed an I/O or HTTP request.
    let cases = [
        (
            "error-storage-permission", "Storage permission · sample fixture", "zg --index .",
            EngineError::permission_denied(format!("open workspace manifest {}: Permission denied (sample)", root.join(".zvec-grep/manifest.json").display())),
        ),
        (
            "error-storage-disk", "Storage capacity · sample fixture", "zg --index .",
            EngineError::storage_failure("failed to finalize index storage: No space left on device (sample)"),
        ),
        (
            "error-remote-authorization", "Remote consent required · sample fixture", "zg --index --embedding qwen/qwen3.7-text-embedding",
            EngineError::permission_denied("Remote embedding authorization required (sample)")
                .with_help("Run: zg --auth grant --capability embedding --scope workspace --embedding qwen/qwen3.7-text-embedding"),
        ),
        (
            "error-remote-authentication", "Remote credentials · sample fixture", "zg --hybrid search",
            EngineError::permission_denied("Embedding request returned an error (sample)\nstatus=401 providerMessage=Invalid API key"),
        ),
        (
            "error-remote-network", "Remote connection · sample fixture", "zg --hybrid search",
            EngineError::internal("Embedding request failed (sample)\nendpoint=https://embedding.example.test\ncaused by: connection refused"),
        ),
        (
            "error-remote-timeout", "Remote deadline · sample fixture", "zg --hybrid search",
            EngineError::deadline_exceeded("Embedding request timed out (sample)\nendpoint=https://embedding.example.test"),
        ),
    ];
    cases
        .into_iter()
        .map(|(id, title, command, error)| error_card(id, title, command, error, options(), true))
        .collect()
}

fn missing_manifest(root: &Path) -> EngineError {
    EngineError::storage_failure(format!(
        "workspace index '{}' has no manifest; rebuild the index with `zg --index --rebuild`",
        root.join(".zvec-grep/index").display(),
    ))
}

fn error_presentations(root: &Path) -> io::Result<Vec<Fixture>> {
    let cancelled = EngineError::cancelled("Index creation cancelled");
    debug_assert_eq!(cancelled.code(), EngineError::CANCELLED);
    Ok(vec![
        error_card(
            "error-debug",
            "Full diagnostics · fixture",
            "zg --index . --debug",
            missing_manifest(root),
            OutputOptions {
                debug: true,
                ..options()
            },
            true,
        )?,
        error_card(
            "error-no-color",
            "Color disabled · fixture",
            "zg --index . --no-color",
            missing_manifest(root),
            OutputOptions {
                color: ColorMode::Never,
                ..options()
            },
            true,
        )?,
        error_card(
            "error-redirected",
            "Redirected diagnostics · fixture",
            "zg --index . 2>error.log",
            missing_manifest(root),
            OutputOptions {
                color: ColorMode::Auto,
                ..options()
            },
            false,
        )?,
        // main() handles CANCELLED before report_error(), using this exact
        // public message renderer. It returns exit status 130 separately.
        render(
            "cancelled",
            "Automatic index cancellation · fixture",
            "zg --hybrid search  [Esc during index creation]",
            |output| {
                zg_cli::write_message(
                    output,
                    MessageKind::Info,
                    "Cancelled",
                    &[],
                    ColorMode::Always,
                    true,
                )
            },
        )?,
    ])
}

fn index_outcomes(root: &Path) -> io::Result<Vec<Fixture>> {
    let empty = IndexResult {
        duration_micros: 12_000,
        ..IndexResult::default()
    };
    let unchanged = IndexResult {
        files_scanned: 4_946,
        files_unchanged: 4_946,
        duration_micros: 1_200_000,
        ..IndexResult::default()
    };
    let failed = IndexResult {
        files_scanned: 3,
        files_added: 2,
        files_failed: 1,
        entities_created: 18,
        duration_micros: 450_000,
        failed_files: vec![FailedFile {
            path: root.join("src/sample.rs"),
            reason: "prepare: invalid text encoding (sample)".into(),
        }],
        ..IndexResult::default()
    };
    let rebuilt = IndexResult {
        files_scanned: 4_946,
        files_added: 4_946,
        entities_created: 155_654,
        duration_micros: 86_774_000,
        ..IndexResult::default()
    };
    let mut fixtures = Vec::new();
    for (id, title, command, result) in [
        (
            "index-empty",
            "No indexable files · fixture",
            "zg --index .",
            empty,
        ),
        (
            "index-up-to-date",
            "No changes · fixture",
            "zg --index .",
            unchanged,
        ),
        (
            "index-failed",
            "Failed files · fixture",
            "zg --index .",
            failed,
        ),
        (
            "index-rebuild",
            "Successful rebuild · fixture",
            "zg --index . --rebuild",
            rebuilt,
        ),
    ] {
        fixtures.push(render(id, title, command, |output| {
            zg_cli::write_index_with_options(output, root, &result, None, options(), true)
        })?);
    }
    fixtures.push(render(
        "index-no-color",
        "Color disabled · fixture",
        "zg --index . --no-color",
        |output| {
            zg_cli::write_index_with_options(
                output,
                root,
                &IndexResult {
                    files_scanned: 4_946,
                    files_added: 4_946,
                    entities_created: 155_654,
                    duration_micros: 86_774_000,
                    ..IndexResult::default()
                },
                None,
                OutputOptions {
                    color: ColorMode::Never,
                    ..options()
                },
                true,
            )
        },
    )?);
    Ok(fixtures)
}

fn status_outcomes(root: &Path) -> io::Result<Vec<Fixture>> {
    let mut fixtures = Vec::new();
    for (id, title, state) in [
        (
            "status-ready",
            "Ready workspace · fixture",
            IndexStatus::Ready,
        ),
        (
            "status-stale",
            "Workspace changes · fixture",
            IndexStatus::Stale,
        ),
        (
            "status-failed",
            "Failed workspace files · fixture",
            IndexStatus::Failed,
        ),
        (
            "status-disabled",
            "Disabled workspace · fixture",
            IndexStatus::Disabled,
        ),
        (
            "status-missing",
            "Missing index · fixture",
            IndexStatus::Missing,
        ),
        (
            "status-uninitialized",
            "Unconfigured workspace · fixture",
            IndexStatus::Uninitialized,
        ),
        (
            "status-unknown",
            "Uninspected workspace · fixture",
            IndexStatus::Unknown,
        ),
        (
            "status-rebuild-required",
            "Incompatible index · fixture",
            IndexStatus::RebuildRequired,
        ),
    ] {
        let info = status_fixture(root, state);
        debug_assert_eq!(info.index_status(), state);
        fixtures.push(render(id, title, "zg --status", |output| {
            zg_cli::write_info_with_options(output, &info, options(), true)
        })?);
    }
    Ok(fixtures)
}

fn status_fixture(root: &Path, state: IndexStatus) -> InfoResult {
    let mut info = info_fixture(root);
    match state {
        IndexStatus::Ready => {}
        IndexStatus::Stale => {
            if let Some(stats) = &mut info.status {
                stats.files_unchanged -= 3;
                stats.files_modified = 3;
                stats.files_pending = 3;
            }
        }
        IndexStatus::Failed => {
            if let Some(stats) = &mut info.status {
                stats.files_unchanged -= 1;
                stats.files_failed = 1;
                stats.failed_files.push(FailedFile {
                    path: root.join("src/sample.rs"),
                    reason: "prepare: invalid text encoding (sample)".into(),
                });
            }
        }
        IndexStatus::Disabled | IndexStatus::Missing | IndexStatus::Uninitialized => {
            info.indexed = false;
            info.compatibility = IndexCompatibility::Unbuilt;
            info.source = InfoSource::Unindexed;
            info.status = None;
            info.workspace_index = None;
            info.index_policy = match state {
                IndexStatus::Disabled => WorkspaceIndexPolicy::Disabled,
                IndexStatus::Uninitialized => WorkspaceIndexPolicy::Uninitialized,
                _ => WorkspaceIndexPolicy::Enabled,
            };
        }
        IndexStatus::Unknown => info.status = None,
        IndexStatus::RebuildRequired => {
            info.status = None;
            info.workspace_index = None;
            info.compatibility = IndexCompatibility::RebuildRequired {
                actual_version: Some(1),
                expected_version: 2,
                reason: "unsupported persisted index version 1 (sample)".into(),
            };
        }
    }
    info
}

fn authorization_outcomes(root: &Path) -> io::Result<Vec<Fixture>> {
    let authorized = AuthorizationStatus {
        root: root.into(),
        path: root.join(".zvec-grep/authorization.json"),
        grants: vec![AuthorizationGrantStatus {
            model: "qwen/qwen3.7-text-embedding".into(),
            endpoint: "https://dashscope.aliyuncs.com/compatible-mode/v1".into(),
            endpoint_host: "dashscope.aliyuncs.com".into(),
        }],
    };
    let missing = AuthorizationStatus {
        grants: vec![],
        ..authorized.clone()
    };
    let mut fixtures = vec![
        render(
            "auth-grant",
            "Grant acknowledged · fixture; no grant written",
            "zg --auth grant --capability embedding --scope workspace --embedding qwen/qwen3.7-text-embedding",
            |output| {
                zg_cli::write_authorization_status(output, &authorized, ColorMode::Always, true)
            },
        )?,
        render(
            "auth-status-authorized",
            "Existing authorization · fixture",
            "zg --auth status",
            |output| {
                zg_cli::write_authorization_status(output, &authorized, ColorMode::Always, true)
            },
        )?,
        render(
            "auth-status-empty",
            "Authorization missing · fixture",
            "zg --auth status",
            |output| zg_cli::write_authorization_status(output, &missing, ColorMode::Always, true),
        )?,
    ];
    for (id, title, count) in [
        (
            "auth-revoke-empty",
            "No grants to revoke · fixture",
            Some(0),
        ),
        (
            "auth-revoke",
            "Grants revoked · fixture; no grant changed",
            Some(2),
        ),
        (
            "auth-revoke-removed",
            "Authorization record removed · fixture",
            None,
        ),
    ] {
        fixtures.push(render(id, title, "zg --auth revoke", |output| {
            zg_cli::write_authorization_revoke_with_options(output, count, ColorMode::Always, true)
        })?);
    }
    Ok(fixtures)
}

fn configuration_outcomes(root: &Path) -> io::Result<Vec<Fixture>> {
    let config = std::env::home_dir()
        .unwrap_or_else(|| root.into())
        .join(".zvec-grep/config.json");
    [
        (
            "config-provider",
            "Provider configured · fixture; no credentials saved",
            "zg --config provider set qwen --api-key <redacted>",
            "Provider",
            "qwen",
        ),
        (
            "config-model",
            "Model configured · fixture; no config changed",
            "zg --config model set local/potion-code-16m-v2 --default",
            "Model",
            "local/potion-code-16m-v2",
        ),
    ]
    .into_iter()
    .map(|(id, title, command, kind, reference)| {
        render(id, title, command, |output| {
            zg_cli::write_config_result(output, kind, reference, &config, ColorMode::Always, true)
        })
    })
    .collect()
}
