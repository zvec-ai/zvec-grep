//! Private lexical search service backed by ripgrep's embedded `grep` crates.

pub(crate) mod structure;
pub(crate) mod types;

use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::UNIX_EPOCH,
};

use crate::{
    EngineError,
    domain::{LineColumnRange, TextPosition},
    utils::{decode_text, utf16_len},
};
use grep::{
    matcher::Matcher,
    regex::{RegexMatcher, RegexMatcherBuilder},
    searcher::{BinaryDetection, SearcherBuilder, sinks::Bytes},
};
use ignore::{WalkBuilder, WalkState, overrides::OverrideBuilder, types::TypesBuilder};
use tokio::sync::Semaphore;
use tracing::debug;

use self::types::{
    LexicalCoverage, LexicalDiagnostics, LexicalMatch, LexicalSearchReply, LexicalSearchRequest,
};

const EMBEDDED_BACKEND: &str = "grep";
const EMBEDDED_COMMAND: &str = "[embedded-grep]";
const HARD_IGNORED_DIRECTORIES: [&str; 2] = [".git", ".zvec-grep"];
const DEFAULT_MAX_SEARCH_THREADS: usize = 12;

#[derive(Clone, Debug)]
pub(crate) struct LexicalSearchService {
    search_slots: Arc<Semaphore>,
    worker_threads: usize,
}

impl LexicalSearchService {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            search_slots: Arc::new(Semaphore::new(1)),
            worker_threads: default_worker_threads(),
        }
    }

    #[must_use]
    pub(crate) fn with_max_searches(mut self, maximum: usize) -> Self {
        self.search_slots = Arc::new(Semaphore::new(maximum.max(1)));
        self
    }

    #[must_use]
    #[cfg(test)]
    fn with_worker_threads(mut self, worker_threads: usize) -> Self {
        self.worker_threads = worker_threads.max(1);
        self
    }
}

impl Default for LexicalSearchService {
    fn default() -> Self {
        Self::new()
    }
}

impl LexicalSearchService {
    pub(crate) async fn search(
        &self,
        root: &Path,
        request: &LexicalSearchRequest,
    ) -> Result<LexicalSearchReply, EngineError> {
        if request.patterns.is_empty() && request.pattern_files.is_empty() {
            return Err(EngineError::invalid_argument(
                "lexical search requires a pattern or pattern file",
            ));
        }

        let _search_slot = self.search_slots.acquire().await.map_err(|_| {
            EngineError::internal("lexical search concurrency limiter closed unexpectedly")
        })?;
        let checked_paths = check_paths(root, &request.paths);
        if !request.paths.is_empty() && checked_paths.existing.is_empty() {
            return Ok(empty_reply(root, request, &checked_paths));
        }

        let worker_threads = worker_threads_for_search(root, request, self.worker_threads);
        let root = root.to_path_buf();
        let request = request.clone();
        run_blocking(move || search_sync(&root, &request, &checked_paths, worker_threads)).await
    }
}

fn default_worker_threads() -> usize {
    // Match ripgrep's automatic search-thread heuristic.
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(DEFAULT_MAX_SEARCH_THREADS)
}

fn worker_threads_for_search(
    root: &Path,
    request: &LexicalSearchRequest,
    configured: usize,
) -> usize {
    if is_single_file_search(root, &request.paths) {
        1
    } else {
        configured.max(1)
    }
}

fn is_single_file_search(root: &Path, paths: &[PathBuf]) -> bool {
    match paths {
        [] => !root.is_dir(),
        [path] => !resolve_path(root, path).is_dir(),
        _ => false,
    }
}

#[derive(Clone, Debug)]
struct CheckedPaths {
    existing: Vec<PathBuf>,
    missing: Vec<PathBuf>,
}

async fn run_blocking<T, F>(function: F) -> Result<T, EngineError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, EngineError> + Send + 'static,
{
    tokio::task::spawn_blocking(function)
        .await
        .map_err(|error| EngineError::internal(format!("embedded grep worker failed: {error}")))?
}

fn search_sync(
    root: &Path,
    request: &LexicalSearchRequest,
    checked_paths: &CheckedPaths,
    worker_threads: usize,
) -> Result<LexicalSearchReply, EngineError> {
    let patterns = load_patterns(root, request)?;
    if patterns.is_empty() {
        return Ok(empty_reply(root, request, checked_paths));
    }
    let matcher = build_matcher(&patterns, request)?;
    let walker = build_walker(root, request, checked_paths, worker_threads)?;
    debug!(
        patterns = patterns.len(),
        paths = checked_paths.existing.len(),
        worker_threads,
        "running embedded grep"
    );
    let mut lexical_matches = if worker_threads == 1 {
        search_paths_serial(root, request, &matcher, &walker)?
    } else {
        search_paths_parallel(root, request, &matcher, &walker)?
    };

    expand_context(&mut lexical_matches, request);
    debug_assert!(lexical_matches.iter().all(|item| {
        item.range
            .contains(item.excerpt_range.as_ref().unwrap_or(&item.range))
    }));
    lexical_matches.sort_by(|left, right| {
        left.relative_path
            .cmp(&right.relative_path)
            .then(left.range.start.line.cmp(&right.range.start.line))
            .then(
                left.range
                    .start
                    .column_utf16
                    .cmp(&right.range.start.column_utf16),
            )
    });

    let truncated = request
        .limit
        .is_some_and(|limit| lexical_matches.len() > limit);
    if let Some(limit) = request.limit {
        lexical_matches.truncate(limit);
    }
    for (index, item) in lexical_matches.iter_mut().enumerate() {
        item.rank = index + 1;
    }

    Ok(LexicalSearchReply {
        root: root.to_path_buf(),
        coverage: if truncated {
            LexicalCoverage::Truncated
        } else {
            LexicalCoverage::Exhaustive
        },
        matches: lexical_matches,
        diagnostics: diagnostics(request, checked_paths, truncated),
    })
}

fn search_paths_serial(
    root: &Path,
    request: &LexicalSearchRequest,
    matcher: &RegexMatcher,
    walker: &WalkBuilder,
) -> Result<Vec<LexicalMatch>, EngineError> {
    let mut lexical_matches = Vec::new();
    let mut searcher = build_searcher();
    for result in walker.build() {
        let entry = result.map_err(|error| {
            EngineError::storage_failure(format!("failed to traverse workspace: {error}"))
        })?;
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let path = entry.into_path();
        if !matches_modified_time(&path, request) {
            continue;
        }
        search_file(root, &path, matcher, &mut searcher, &mut lexical_matches)?;
    }
    Ok(lexical_matches)
}

fn search_paths_parallel(
    root: &Path,
    request: &LexicalSearchRequest,
    matcher: &RegexMatcher,
    walker: &WalkBuilder,
) -> Result<Vec<LexicalMatch>, EngineError> {
    let lexical_matches = Mutex::new(Vec::new());
    let first_error = Mutex::new(None);
    let stopped = AtomicBool::new(false);
    walker.build_parallel().run(|| {
        let lexical_matches = &lexical_matches;
        let first_error = &first_error;
        let stopped = &stopped;
        let mut searcher = build_searcher();
        Box::new(move |result| {
            if stopped.load(Ordering::Acquire) {
                return WalkState::Quit;
            }
            let entry = match result {
                Ok(entry) => entry,
                Err(error) => {
                    return stop_parallel_search(
                        first_error,
                        stopped,
                        EngineError::storage_failure(format!(
                            "failed to traverse workspace: {error}"
                        )),
                    );
                }
            };
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                return WalkState::Continue;
            }
            let path = entry.into_path();
            if !matches_modified_time(&path, request) {
                return WalkState::Continue;
            }

            let mut file_matches = Vec::new();
            if let Err(error) = search_file(root, &path, matcher, &mut searcher, &mut file_matches)
            {
                return stop_parallel_search(first_error, stopped, error);
            }
            if file_matches.is_empty() {
                return WalkState::Continue;
            }
            let Ok(mut all_matches) = lexical_matches.lock() else {
                return stop_parallel_search(
                    first_error,
                    stopped,
                    EngineError::internal("embedded grep result collector was poisoned"),
                );
            };
            all_matches.extend(file_matches);
            WalkState::Continue
        })
    });

    let first_error = first_error
        .into_inner()
        .map_err(|_| EngineError::internal("embedded grep error collector was poisoned"))?;
    if let Some(error) = first_error {
        return Err(error);
    }
    lexical_matches
        .into_inner()
        .map_err(|_| EngineError::internal("embedded grep result collector was poisoned"))
}

fn build_searcher() -> grep::searcher::Searcher {
    SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\0'))
        .line_number(true)
        .build()
}

fn stop_parallel_search(
    first_error: &Mutex<Option<EngineError>>,
    stopped: &AtomicBool,
    error: EngineError,
) -> WalkState {
    if let Ok(mut first_error) = first_error.lock()
        && first_error.is_none()
    {
        *first_error = Some(error);
    }
    stopped.store(true, Ordering::Release);
    WalkState::Quit
}

fn load_patterns(root: &Path, request: &LexicalSearchRequest) -> Result<Vec<String>, EngineError> {
    let mut patterns = request.patterns.clone();
    for pattern_file in &request.pattern_files {
        let path = resolve_path(root, pattern_file);
        let file_patterns = grep::cli::patterns_from_path(&path).map_err(|error| {
            EngineError::storage_failure(format!(
                "pattern file {} could not be read: {error}",
                pattern_file.display()
            ))
        })?;
        patterns.extend(file_patterns);
    }
    let mut seen = HashSet::new();
    patterns.retain(|pattern| seen.insert(pattern.clone()));
    Ok(patterns)
}

fn build_matcher(
    patterns: &[String],
    request: &LexicalSearchRequest,
) -> Result<RegexMatcher, EngineError> {
    let mut builder = RegexMatcherBuilder::new();
    builder
        .multi_line(true)
        .line_terminator(Some(b'\n'))
        .case_insensitive(request.options.ignore_case)
        .fixed_strings(request.options.fixed_strings)
        .word(request.options.word_regexp);
    builder.build_many(patterns).map_err(|error| {
        EngineError::invalid_argument(format!("invalid lexical search pattern: {error}"))
    })
}

fn build_walker(
    root: &Path,
    request: &LexicalSearchRequest,
    checked_paths: &CheckedPaths,
    worker_threads: usize,
) -> Result<WalkBuilder, EngineError> {
    let paths = if request.paths.is_empty() {
        vec![root.to_path_buf()]
    } else {
        checked_paths
            .existing
            .iter()
            .map(|path| resolve_path(root, path))
            .collect()
    };
    let mut walker = WalkBuilder::from_iter(paths);
    walker
        .current_dir(root)
        .hidden(!request.options.hidden)
        .follow_links(request.options.follow)
        .threads(worker_threads)
        .max_depth(request.options.max_depth)
        .max_filesize(request.options.max_file_size_bytes);

    if request.options.hidden {
        let filter_root = root.to_path_buf();
        walker.filter_entry(move |entry| {
            let path = entry
                .path()
                .strip_prefix(&filter_root)
                .unwrap_or_else(|_| entry.path());
            !is_hard_ignored_path(path)
        });
    }

    if request.options.no_ignore {
        walker
            .parents(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false);
    } else {
        walker.add_custom_ignore_filename(".rgignore");
    }

    for ignore_file in &request.options.ignore_files {
        let path = resolve_path(root, ignore_file);
        if let Some(error) = walker.add_ignore(&path) {
            return Err(EngineError::storage_failure(format!(
                "ignore file {} could not be loaded: {error}",
                ignore_file.display()
            )));
        }
    }

    if !request.options.globs.is_empty() {
        let mut overrides = OverrideBuilder::new(root);
        for glob in &request.options.globs {
            overrides.add(glob).map_err(|error| {
                EngineError::invalid_argument(format!("invalid glob {glob:?}: {error}"))
            })?;
        }
        walker.overrides(overrides.build().map_err(|error| {
            EngineError::invalid_argument(format!("invalid glob override: {error}"))
        })?);
    }

    if !request.options.file_types.is_empty() || !request.options.excluded_file_types.is_empty() {
        walker.types(build_file_types(request)?);
    }
    Ok(walker)
}

fn build_file_types(request: &LexicalSearchRequest) -> Result<ignore::types::Types, EngineError> {
    let mut builder = TypesBuilder::new();
    builder.add_defaults();
    for name in &request.options.file_types {
        builder.select(name);
    }
    for name in &request.options.excluded_file_types {
        builder.negate(name);
    }
    builder.build().map_err(|error| {
        EngineError::invalid_argument(format!("invalid ripgrep file type selection: {error}"))
    })
}

fn search_file(
    root: &Path,
    path: &Path,
    matcher: &RegexMatcher,
    searcher: &mut grep::searcher::Searcher,
    results: &mut Vec<LexicalMatch>,
) -> Result<(), EngineError> {
    let absolute_path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let relative_path = absolute_path
        .strip_prefix(root)
        .map_or_else(|_| absolute_path.clone(), Path::to_path_buf);
    let search_result = searcher.search_path(
        matcher,
        &absolute_path,
        Bytes(|line_number, bytes| {
            let first = matcher.find(bytes).map_err(io::Error::other)?;
            let Some(first) = first else {
                return Ok(true);
            };
            let content_bytes = trim_line_terminator(bytes);
            let Ok(content) = std::str::from_utf8(content_bytes) else {
                return Ok(true);
            };
            let line_number = usize::try_from(line_number)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let start = text_position_at_byte_offset(content, first.start());
            let end = text_position_at_byte_offset(content, first.end());
            results.push(LexicalMatch {
                rank: 0,
                absolute_path: absolute_path.clone(),
                relative_path: relative_path.clone(),
                range: LineColumnRange {
                    start: TextPosition {
                        line: line_number + start.0,
                        column_utf16: start.1,
                    },
                    end: TextPosition {
                        line: line_number + end.0,
                        column_utf16: end.1,
                    },
                },
                excerpt_range: None,
                content: content.to_owned(),
            });
            Ok(true)
        }),
    );
    if let Err(error) = search_result {
        return Err(EngineError::from_io(
            format!("failed to search {}", absolute_path.display()),
            &error,
        ));
    }
    Ok(())
}

fn trim_line_terminator(mut bytes: &[u8]) -> &[u8] {
    if let Some(stripped) = bytes.strip_suffix(b"\n") {
        bytes = stripped;
    }
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

fn check_paths(root: &Path, paths: &[PathBuf]) -> CheckedPaths {
    let (existing, missing) = paths
        .iter()
        .cloned()
        .partition(|path| resolve_path(root, path).exists());
    CheckedPaths { existing, missing }
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn is_hard_ignored_path(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(name) = component else {
            return false;
        };
        HARD_IGNORED_DIRECTORIES
            .iter()
            .any(|directory| name == *directory)
    })
}

fn empty_reply(
    root: &Path,
    request: &LexicalSearchRequest,
    checked_paths: &CheckedPaths,
) -> LexicalSearchReply {
    LexicalSearchReply {
        root: root.to_path_buf(),
        coverage: LexicalCoverage::Exhaustive,
        matches: Vec::new(),
        diagnostics: diagnostics(request, checked_paths, false),
    }
}

fn diagnostics(
    request: &LexicalSearchRequest,
    checked_paths: &CheckedPaths,
    truncated: bool,
) -> LexicalDiagnostics {
    LexicalDiagnostics {
        backend: EMBEDDED_BACKEND.to_owned(),
        command: PathBuf::from(EMBEDDED_COMMAND),
        args: diagnostic_args(request),
        ignored_directories: HARD_IGNORED_DIRECTORIES
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        missing_paths: checked_paths.missing.clone(),
        searched_paths: checked_paths.existing.clone(),
        limit: request.limit,
        truncated,
    }
}

fn diagnostic_args(request: &LexicalSearchRequest) -> Vec<String> {
    let mut args = Vec::new();
    push_diagnostic_switch(&mut args, request.options.fixed_strings, "--fixed-strings");
    push_diagnostic_switch(&mut args, request.options.ignore_case, "--ignore-case");
    push_diagnostic_switch(&mut args, request.options.word_regexp, "--word-regexp");
    push_diagnostic_switch(&mut args, request.options.hidden, "--hidden");
    push_diagnostic_switch(&mut args, request.options.no_ignore, "--no-ignore");
    push_diagnostic_switch(&mut args, request.options.follow, "--follow");
    push_diagnostic_value(
        &mut args,
        "--max-depth",
        request.options.max_depth.map(|value| value.to_string()),
    );
    push_diagnostic_value(
        &mut args,
        "--max-filesize",
        request
            .options
            .max_file_size_bytes
            .map(|value| value.to_string()),
    );
    for glob in &request.options.globs {
        args.extend(["--glob".to_owned(), glob.clone()]);
    }
    for file_type in &request.options.file_types {
        args.extend(["--type".to_owned(), file_type.clone()]);
    }
    for file_type in &request.options.excluded_file_types {
        args.extend(["--type-not".to_owned(), file_type.clone()]);
    }
    for pattern in &request.patterns {
        args.extend(["--regexp".to_owned(), pattern.clone()]);
    }
    for pattern_file in &request.pattern_files {
        args.extend([
            "--file".to_owned(),
            pattern_file.to_string_lossy().into_owned(),
        ]);
    }
    args
}

fn push_diagnostic_switch(args: &mut Vec<String>, enabled: bool, name: &str) {
    if enabled {
        args.push(name.to_owned());
    }
}

fn push_diagnostic_value(args: &mut Vec<String>, name: &str, value: Option<String>) {
    if let Some(value) = value {
        args.extend([name.to_owned(), value]);
    }
}

fn matches_modified_time(path: &Path, request: &LexicalSearchRequest) -> bool {
    let Some(after) = request.options.modified_after_epoch_ms else {
        return request
            .options
            .modified_before_epoch_ms
            .is_none_or(|before| modified_epoch_ms(path).is_some_and(|value| value <= before));
    };
    let Some(modified) = modified_epoch_ms(path) else {
        return false;
    };
    modified >= after
        && request
            .options
            .modified_before_epoch_ms
            .is_none_or(|before| modified <= before)
}

fn expand_context(matches: &mut [LexicalMatch], request: &LexicalSearchRequest) {
    let before = request.options.before_context;
    let after = request.options.after_context;
    if before == 0 && after == 0 {
        return;
    }

    let mut cache: HashMap<PathBuf, Option<Vec<String>>> = HashMap::new();
    for item in matches {
        let lines = cache.entry(item.absolute_path.clone()).or_insert_with(|| {
            std::fs::read(&item.absolute_path).ok().and_then(|bytes| {
                decode_text(&bytes, true)
                    .map(|content| content.lines().map(str::to_owned).collect())
            })
        });
        let Some(lines) = lines else {
            continue;
        };
        if lines.is_empty() {
            continue;
        }

        let excerpt = item.range;
        let start_line = excerpt.start.line.saturating_sub(before).max(1);
        let end_line = excerpt.end.line.saturating_add(after).min(lines.len());
        let content = lines[start_line - 1..end_line].join("\n");
        let end_offset = utf16_len(&lines[end_line - 1]);
        item.excerpt_range = Some(excerpt);
        item.range = LineColumnRange {
            start: TextPosition {
                line: start_line,
                column_utf16: 0,
            },
            end: TextPosition {
                line: end_line,
                column_utf16: end_offset,
            },
        };
        item.content = content;
    }
}

fn modified_epoch_ms(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn text_position_at_byte_offset(value: &str, byte_offset: usize) -> (usize, usize) {
    let end = byte_offset.min(value.len());
    let prefix = String::from_utf8_lossy(&value.as_bytes()[..end]);
    let line_offset = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let last = prefix.rsplit('\n').next().unwrap_or_default();
    (line_offset, utf16_len(last.trim_end_matches('\r')))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use super::types::{LexicalOptions, LexicalSearchReply, LexicalSearchRequest};
    use tempfile::TempDir;

    use super::{
        DEFAULT_MAX_SEARCH_THREADS, LexicalSearchService, default_worker_threads,
        worker_threads_for_search,
    };

    fn request(pattern: &str) -> LexicalSearchRequest {
        LexicalSearchRequest {
            patterns: vec![pattern.to_owned()],
            ..LexicalSearchRequest::default()
        }
    }

    async fn search(
        service: &LexicalSearchService,
        root: &Path,
        request: &LexicalSearchRequest,
    ) -> LexicalSearchReply {
        service
            .search(root, request)
            .await
            .expect("embedded search")
    }

    #[test]
    fn default_threads_match_ripgrep_heuristic() {
        let expected = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(DEFAULT_MAX_SEARCH_THREADS);
        assert_eq!(default_worker_threads(), expected);
    }

    #[test]
    fn single_file_search_forces_one_worker() {
        let root = TempDir::new().expect("temp dir");
        fs::write(root.path().join("a.txt"), "needle\n").expect("fixture");
        fs::create_dir(root.path().join("src")).expect("fixture directory");

        let mut single_file = request("needle");
        single_file.paths = vec![PathBuf::from("a.txt")];
        assert_eq!(worker_threads_for_search(root.path(), &single_file, 8), 1);

        let mut single_directory = request("needle");
        single_directory.paths = vec![PathBuf::from("src")];
        assert_eq!(
            worker_threads_for_search(root.path(), &single_directory, 8),
            8
        );

        let mut multiple_paths = request("needle");
        multiple_paths.paths = vec![PathBuf::from("a.txt"), PathBuf::from("src")];
        assert_eq!(
            worker_threads_for_search(root.path(), &multiple_paths, 8),
            8
        );
        assert_eq!(
            worker_threads_for_search(&root.path().join("a.txt"), &request("needle"), 8),
            1
        );
    }

    #[tokio::test]
    async fn searches_in_process_and_reports_utf16_columns() {
        let root = TempDir::new().expect("temp dir");
        std::fs::write(
            root.path().join("a.txt"),
            "before\nlet x = \"你好\";\nafter\n",
        )
        .expect("fixture");
        let reply = search(&LexicalSearchService::new(), root.path(), &request("你好")).await;

        assert_eq!(reply.diagnostics.backend, "grep");
        assert_eq!(reply.matches.len(), 1);
        assert_eq!(reply.matches[0].relative_path, Path::new("a.txt"));
        assert_eq!(reply.matches[0].range.start.line, 2);
        assert_eq!(reply.matches[0].range.start.column_utf16, 9);
        assert_eq!(reply.matches[0].range.end.column_utf16, 11);
    }

    #[tokio::test]
    async fn honors_ignore_hidden_glob_type_and_hard_exclusions() {
        let root = TempDir::new().expect("temp dir");
        fs::create_dir_all(root.path().join(".git")).expect("git dir");
        fs::create_dir_all(root.path().join(".hidden")).expect("hidden dir");
        fs::write(root.path().join(".git/config"), "needle").expect("git fixture");
        fs::write(root.path().join(".hidden/keep.rs"), "needle").expect("hidden fixture");
        fs::write(root.path().join("keep.rs"), "needle").expect("rust fixture");
        fs::write(root.path().join("drop.txt"), "needle").expect("text fixture");
        fs::write(root.path().join(".gitignore"), "ignored.rs\n").expect("ignore fixture");
        fs::write(root.path().join("ignored.rs"), "needle").expect("ignored fixture");

        let mut typed = request("needle");
        typed.options = LexicalOptions {
            hidden: true,
            file_types: vec!["rust".to_owned()],
            ..LexicalOptions::default()
        };
        let reply = search(&LexicalSearchService::new(), root.path(), &typed).await;
        let paths = reply
            .matches
            .iter()
            .map(|item| item.relative_path.as_path())
            .collect::<Vec<_>>();
        assert_eq!(paths, [Path::new(".hidden/keep.rs"), Path::new("keep.rs")]);

        typed.options.globs = vec!["!/keep.rs".to_owned()];
        let reply = search(&LexicalSearchService::new(), root.path(), &typed).await;
        assert_eq!(reply.matches.len(), 1);
        assert_eq!(reply.matches[0].relative_path, Path::new(".hidden/keep.rs"));
    }

    #[tokio::test]
    async fn supports_fixed_word_case_patterns_files_context_and_limits() {
        let root = TempDir::new().expect("temp dir");
        fs::write(
            root.path().join("a.txt"),
            "before\nNeedle.+ exact\nneedle.+ suffix\nafter\n",
        )
        .expect("fixture");
        fs::write(root.path().join("patterns"), "needle.+\n").expect("patterns");
        let mut request = LexicalSearchRequest {
            pattern_files: vec![Path::new("patterns").to_path_buf()],
            limit: Some(1),
            ..LexicalSearchRequest::default()
        };
        request.options = LexicalOptions {
            fixed_strings: true,
            ignore_case: true,
            word_regexp: true,
            before_context: 1,
            after_context: 1,
            ..LexicalOptions::default()
        };

        let reply = search(&LexicalSearchService::new(), root.path(), &request).await;
        assert_eq!(reply.matches.len(), 1);
        assert!(reply.diagnostics.truncated);
        assert_eq!(reply.matches[0].range.start.line, 1);
        assert_eq!(
            reply.matches[0]
                .excerpt_range
                .as_ref()
                .expect("excerpt")
                .start
                .line,
            2
        );
        assert_eq!(
            reply.matches[0].content,
            "before\nNeedle.+ exact\nneedle.+ suffix"
        );
    }

    #[tokio::test]
    async fn supports_line_anchors_and_deduplicates_patterns() {
        let root = TempDir::new().expect("temp dir");
        fs::write(root.path().join("a.txt"), "prefix foo\nfoo\nfoo suffix\n").expect("fixture");
        fs::write(root.path().join("patterns"), "^foo$\n^foo$\n").expect("patterns");
        let request = LexicalSearchRequest {
            patterns: vec!["^foo$".to_owned()],
            pattern_files: vec![Path::new("patterns").to_path_buf()],
            ..LexicalSearchRequest::default()
        };

        let reply = search(&LexicalSearchService::new(), root.path(), &request).await;
        assert_eq!(reply.matches.len(), 1);
        assert_eq!(reply.matches[0].range.start.line, 2);
        assert_eq!(reply.matches[0].content, "foo");
    }

    #[tokio::test]
    async fn parallel_search_keeps_results_in_deterministic_path_order() {
        let root = TempDir::new().expect("temp dir");
        for index in (0..32).rev() {
            let directory = root.path().join(format!("dir-{index:02}"));
            fs::create_dir(&directory).expect("fixture directory");
            fs::write(directory.join("match.txt"), "needle\n").expect("fixture file");
        }

        let reply = search(
            &LexicalSearchService::new().with_worker_threads(4),
            root.path(),
            &request("needle"),
        )
        .await;
        let actual = reply
            .matches
            .iter()
            .map(|item| item.relative_path.clone())
            .collect::<Vec<_>>();
        let expected = (0..32)
            .map(|index| PathBuf::from(format!("dir-{index:02}/match.txt")))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn no_ignore_disables_gitignore_ignore_and_rgignore_files() {
        let root = TempDir::new().expect("temp dir");
        fs::create_dir(root.path().join(".git")).expect("git repository marker");
        fs::write(root.path().join(".gitignore"), "git.txt\n").expect("gitignore");
        fs::write(root.path().join(".ignore"), "ignore.txt\n").expect("ignore");
        fs::write(root.path().join(".rgignore"), "rg.txt\n").expect("rgignore");
        for name in ["git.txt", "ignore.txt", "rg.txt"] {
            fs::write(root.path().join(name), "needle\n").expect("fixture");
        }

        let ignored = search(
            &LexicalSearchService::new(),
            root.path(),
            &request("needle"),
        )
        .await;
        assert!(ignored.matches.is_empty());

        let mut unfiltered = request("needle");
        unfiltered.options.no_ignore = true;
        let reply = search(&LexicalSearchService::new(), root.path(), &unfiltered).await;
        assert_eq!(reply.matches.len(), 3);
    }
}
