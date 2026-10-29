//! Captures production CLI renderers with explicit fixture data for review images.
//!
//! From `rust/`:
//! `TERM=xterm-256color COLUMNS=100 cargo run -p zg-cli --example presentation > presentation.json`
//!
//! This does not index files, configure providers, grant permissions, or contact
//! a service. Counts and command results are fixtures. Progress frames come from
//! the real animation thread; the optimization capture takes about 4.2 seconds.
//! Consumers should retain the fixture labels when publishing screenshots.

use std::{
    error::Error,
    io::{self, Cursor, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use serde::Serialize;
use zg_cli::{ColorMode, IndexProgressDisplay, OutputOptions};
use zg_engine::{
    EngineError,
    api::{
        context::{
            ContextResult,
            result::{
                ContextCoverage, ContextDiagnostics, ContextIndexingStatus, ContextSource,
                ContextWorkspaceIndex,
            },
        },
        index::{
            IndexResult,
            options::ScanRules,
            progress::{
                DETAIL_FINALIZING_INDEX, DETAIL_SAVING_INDEX, IndexProgress, IndexProgressPhase,
            },
        },
        info::{
            InfoResult,
            result::{
                FailedFile, IndexCompatibility, IndexStats, InfoSource, WorkspaceIndexEmbedding,
                WorkspaceIndexFts, WorkspaceIndexInfo, WorkspaceIndexPolicy,
            },
        },
    },
    authorization::{AuthorizationGrantStatus, AuthorizationStatus, IndexAuthorization},
};

#[path = "presentation/failures.rs"]
mod failures;
#[path = "presentation/search.rs"]
mod search;
#[path = "presentation/upgrade.rs"]
mod upgrade;

#[derive(Serialize)]
struct Fixture {
    id: &'static str,
    title: &'static str,
    command: &'static str,
    ansi: String,
}

fn main() -> Result<(), Box<dyn Error>> {
    if std::env::var("TERM").as_deref() == Ok("dumb") {
        return Err("Use TERM=xterm-256color to capture animated presentation fixtures".into());
    }
    let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("/home/demo"));
    let root = home.join("ai-workspace/zvec-grep");
    let mut fixtures = progress_fixtures()?;
    fixtures.extend(index_fixtures(&root)?);
    fixtures.extend(management_fixtures(&root, &home)?);
    fixtures.extend(interaction_fixtures(&root)?);
    fixtures.extend(search::fixtures(&root)?);
    fixtures.extend(failures::fixtures(&root)?);
    fixtures.extend(upgrade::fixtures(&root)?);
    let mut output = io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, &fixtures)?;
    writeln!(output)?;
    Ok(())
}

fn options() -> OutputOptions {
    OutputOptions {
        color: ColorMode::Always,
        ..OutputOptions::default()
    }
}

fn render(
    id: &'static str,
    title: &'static str,
    command: &'static str,
    draw: impl FnOnce(&mut Vec<u8>) -> io::Result<()>,
) -> io::Result<Fixture> {
    let mut bytes = Vec::new();
    draw(&mut bytes)?;
    Ok(Fixture {
        id,
        title,
        command,
        ansi: String::from_utf8(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
    })
}

#[derive(Clone, Default)]
struct FrameCapture(Arc<(Mutex<Vec<String>>, Condvar)>);

impl Write for FrameCapture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let frame = String::from_utf8(bytes.to_vec())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.0
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(frame);
        self.0.1.notify_one();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl FrameCapture {
    fn wait_for(&self, matches: impl Fn(&str) -> bool) -> io::Result<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut frames = self
            .0
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(frame) = frames.iter().rev().find(|frame| matches(frame)) {
                // Keep the production text and SGR colors verbatim. Only discard
                // cursor management, which has no meaning in a static screenshot.
                let mut snapshot = frame
                    .replace("\x1b[?25l", "")
                    .replace("\x1b[?25h", "")
                    .replace("\x1b[K", "")
                    .replace('\r', "");
                snapshot.push('\n');
                return Ok(snapshot);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "progress renderer did not produce the requested fixture frame",
                ));
            }
            (frames, _) = self
                .0
                .1
                .wait_timeout(frames, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

fn progress_fixtures() -> io::Result<Vec<Fixture>> {
    let capture = FrameCapture::default();
    let display = IndexProgressDisplay::new(capture.clone(), true, ColorMode::Always);
    let reporter = display.reporter();
    let mut progress = IndexProgress {
        phase: IndexProgressPhase::Indexing,
        files_total: Some(36_718),
        files_indexed: Some(18_647),
        files_failed: None,
        detail: None,
        embedding: None,
    };
    reporter.report(progress.clone());
    let mut fixtures = vec![Fixture {
        id: "indexing",
        title: "File processing · fixture",
        command: "zg --index .",
        ansi: capture
            .wait_for(|frame| frame.contains("Indexing files") && frame.contains("51%"))?,
    }];
    progress.files_indexed = progress.files_total;
    progress.detail = Some(DETAIL_FINALIZING_INDEX.into());
    reporter.report(progress.clone());
    fixtures.push(Fixture {
        id: "optimizing",
        title: "Optimization · fixture, real timer",
        command: "zg --index .",
        ansi: capture.wait_for(|frame| {
            frame.contains("Optimizing index")
                && frame.split_whitespace().any(|token| {
                    token
                        .strip_suffix('s')
                        .and_then(|value| value.parse::<f64>().ok())
                        .is_some_and(|seconds| seconds >= 4.2)
                })
        })?,
    });
    progress.detail = Some(DETAIL_SAVING_INDEX.into());
    reporter.report(progress.clone());
    fixtures.push(Fixture {
        id: "saving",
        title: "Saving · fixture",
        command: "zg --index .",
        ansi: capture.wait_for(|frame| frame.contains("Saving index"))?,
    });
    progress.phase = IndexProgressPhase::Done;
    reporter.report(progress);
    fixtures.push(Fixture {
        id: "finishing",
        title: "Final cleanup · fixture",
        command: "zg --index .",
        ansi: capture.wait_for(|frame| frame.contains("Finishing index"))?,
    });
    display.finish();
    Ok(fixtures)
}

fn index_fixtures(root: &Path) -> io::Result<Vec<Fixture>> {
    let mut result = IndexResult {
        files_scanned: 4_946,
        files_added: 4_946,
        entities_created: 155_654,
        duration_micros: 86_774_000,
        ..IndexResult::default()
    };
    let mut fixtures = vec![render(
        "ready",
        "Completed index · fixture",
        "zg --index .",
        |output| zg_cli::write_index_with_options(output, root, &result, None, options(), true),
    )?];
    result.files_added -= 1;
    result.files_failed = 1;
    result.failed_files.push(FailedFile {
        path: root.join("src/broken.rs"),
        reason: "Cannot decode source as UTF-8".into(),
    });
    fixtures.push(render(
        "partial",
        "Partial failure · fixture",
        "zg --index .",
        |output| zg_cli::write_index_with_options(output, root, &result, None, options(), true),
    )?);
    let report = EngineError::storage_failure(format!(
        "workspace index '{}' has no manifest; rebuild the index with `zg --index --rebuild`",
        root.join(".zvec-grep/index").display(),
    ))
    .report();
    fixtures.push(render(
        "error",
        "Actionable error · fixture",
        "zg --index .",
        |output| zg_cli::write_error_report(output, &report, Some(false), options(), true),
    )?);
    fixtures.push(render(
        "drop",
        "Index removal · fixture",
        "zg --index --drop --yes",
        |output| zg_cli::write_index_drop_with_options(output, root, true, ColorMode::Always, true),
    )?);
    Ok(fixtures)
}

fn info_fixture(root: &Path) -> InfoResult {
    let home = root.join(".zvec-grep");
    InfoResult {
        root: root.into(),
        indexed: true,
        compatibility: IndexCompatibility::Compatible { version: 2 },
        index_policy: WorkspaceIndexPolicy::Enabled,
        index_path: home.join("index"),
        home: home.clone(),
        source: InfoSource::Index,
        workspace_index: Some(WorkspaceIndexInfo {
            path: home,
            root: root.into(),
            scan: ScanRules::default(),
            policy: WorkspaceIndexPolicy::Enabled,
            embedding: Some(WorkspaceIndexEmbedding {
                provider: "local".into(),
                model: "potion-code-16m-v2".into(),
                dimension: 256,
                metric: "cosine".into(),
            }),
            fts: Some(WorkspaceIndexFts {
                tokenizer: "jieba".into(),
                filters: vec!["lowercase".into()],
            }),
            index_version: Some(2),
            created_epoch_ms: 0,
            updated_epoch_ms: 0,
        }),
        status: Some(IndexStats {
            files_scanned: 4_946,
            files_stored: 4_946,
            files_indexed: 4_946,
            files_unchanged: 4_946,
            entities_indexed: 155_654,
            indexed_size_bytes: 9_431_040,
            ..IndexStats::default()
        }),
        suggestion: None,
    }
}

fn management_fixtures(root: &Path, home: &Path) -> io::Result<Vec<Fixture>> {
    let info = info_fixture(root);
    let status = AuthorizationStatus {
        root: root.into(),
        path: root.join(".zvec-grep/authorization.json"),
        grants: vec![AuthorizationGrantStatus {
            model: "qwen/qwen3.7-text-embedding".into(),
            endpoint: "https://dashscope.aliyuncs.com/compatible-mode/v1".into(),
            endpoint_host: "dashscope.aliyuncs.com".into(),
        }],
    };
    Ok(vec![
        render(
            "status",
            "Workspace status · fixture",
            "zg --status",
            |output| zg_cli::write_info_with_options(output, &info, options(), true),
        )?,
        render(
            "config",
            "Model configuration · fixture",
            "zg --config model set local/potion-code-16m-v2 --default",
            |output| {
                zg_cli::write_config_result(
                    output,
                    "Model",
                    "local/potion-code-16m-v2",
                    &home.join(".zvec-grep/config.json"),
                    ColorMode::Always,
                    true,
                )
            },
        )?,
        render(
            "auth",
            "Workspace authorization · fixture",
            "zg --auth status",
            |output| zg_cli::write_authorization_status(output, &status, ColorMode::Always, true),
        )?,
    ])
}

fn interaction_fixtures(root: &Path) -> io::Result<Vec<Fixture>> {
    let target = IndexAuthorization {
        root: root.into(),
        workspace_roots: vec![root.into()],
        model: "qwen/qwen3.7-text-embedding".into(),
        endpoint: "https://dashscope.aliyuncs.com/compatible-mode/v1".into(),
        endpoint_host: "dashscope.aliyuncs.com".into(),
    };
    let query = ContextResult {
        query: "index progress".into(),
        freshness: Some("possibly_stale".into()),
        background_refresh: None,
        indexing: Some(ContextIndexingStatus {
            state: "running".into(),
            completed: Some(120),
            total: Some(240),
        }),
        root: root.into(),
        source: ContextSource::Index,
        coverage: ContextCoverage::RankedSample,
        workspace_index: Some(ContextWorkspaceIndex {
            root: root.into(),
            path: root.join(".zvec-grep"),
        }),
        items: Vec::new(),
        group_results: Vec::new(),
        diagnostics: ContextDiagnostics::default(),
    };
    Ok(vec![
        render(
            "consent",
            "Remote consent interaction · fixture",
            "zg --index --embedding qwen/qwen3.7-text-embedding",
            |output| {
                // A blank fixture response cancels. This exercises presentation only;
                // no authorization is stored and no remote request can be made.
                let _decision =
                    zg_cli::prompt_index_authorization(&target, Cursor::new(b"\n"), output)?;
                Ok(())
            },
        )?,
        render(
            "consent-query",
            "Query consent with FTS-only fallback · fixture",
            "zg --hybrid 'index progress'",
            |output| {
                let _decision = zg_cli::prompt_query_authorization(
                    &target,
                    true,
                    true,
                    Cursor::new(b"\n"),
                    output,
                )?;
                Ok(())
            },
        )?,
        render(
            "refresh",
            "Query freshness · fixture",
            "zg --refresh background 'index progress'",
            |output| zg_cli::write_context_status_with_options(output, &query, options(), true),
        )?,
        render(
            "refresh-off",
            "Stale index with refresh disabled · fixture",
            "zg --refresh off 'index progress'",
            |output| {
                let mut query = query.clone();
                query.indexing = None;
                query.background_refresh = Some("off".into());
                zg_cli::write_context_status_with_options(output, &query, options(), true)
            },
        )?,
    ])
}
