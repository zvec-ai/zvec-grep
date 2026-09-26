//! File scanning and matching backed by ripgrep's embedded `grep` crates.

pub(crate) mod types;

use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, Read},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::UNIX_EPOCH,
};

use crate::{
    EngineError,
    domain::{Range, TextRange},
    utils::{decode_text, line_byte_offsets},
};
use grep::{
    matcher::Matcher,
    regex::{RegexMatcher, RegexMatcherBuilder},
    searcher::{BinaryDetection, SearcherBuilder, Sink, SinkMatch},
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

        let signal = request.signal.as_ref().map_or_else(
            tokio_util::sync::CancellationToken::new,
            tokio_util::sync::CancellationToken::child_token,
        );
        let _cancel_on_drop = signal.clone().drop_guard();
        if signal.is_cancelled() {
            return Err(EngineError::cancelled("lexical search was cancelled"));
        }
        let search_slot = tokio::select! {
            biased;
            () = signal.cancelled() => return Err(EngineError::cancelled("lexical search was cancelled")),
            permit = Arc::clone(&self.search_slots).acquire_owned() => permit.map_err(|_| EngineError::internal("lexical search concurrency limiter closed unexpectedly"))?,
        };
        let checked_paths = check_paths(root, &request.paths);
        if !request.paths.is_empty() && checked_paths.existing.is_empty() {
            return Ok(empty_reply(root, request, &checked_paths));
        }

        let worker_threads = worker_threads_for_search(root, request, self.worker_threads);
        let root = root.to_path_buf();
        let mut request = request.clone();
        request.signal = Some(signal);
        run_blocking(move || {
            // A cancelled caller must not release admission while its worker is still running.
            let _search_slot = search_slot;
            search_sync(&root, &request, &checked_paths, worker_threads)
        })
        .await
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
        request
            .options
            .matching
            .threads
            .filter(|threads| *threads > 0)
            .unwrap_or(configured)
            .max(1)
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

fn check_cancelled(request: &LexicalSearchRequest) -> Result<(), EngineError> {
    if request
        .signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        Err(EngineError::cancelled("lexical search was cancelled"))
    } else {
        Ok(())
    }
}

fn search_sync(
    root: &Path,
    request: &LexicalSearchRequest,
    checked_paths: &CheckedPaths,
    worker_threads: usize,
) -> Result<LexicalSearchReply, EngineError> {
    check_cancelled(request)?;
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
    let count_truncated = AtomicBool::new(false);
    let mut lexical_matches = if worker_threads == 1 {
        search_paths_serial(root, request, &matcher, &walker, &count_truncated)?
    } else {
        search_paths_parallel(root, request, &matcher, &walker, &count_truncated)?
    };

    check_cancelled(request)?;
    expand_context(&mut lexical_matches, request);
    debug_assert!(lexical_matches.iter().all(|item| {
        Range::Text(item.range)
            .contains(&Range::Text(item.excerpt_range.unwrap_or(item.range)))
            .expect("text ranges have the same kind")
    }));
    lexical_matches.sort_by(|left, right| {
        left.relative_path.cmp(&right.relative_path).then(
            left.range
                .start_byte_offset()
                .cmp(&right.range.start_byte_offset()),
        )
    });

    let truncated = count_truncated.load(Ordering::Relaxed)
        || (request.options.matching.stop_on_nonmatch && !lexical_matches.is_empty())
        || request
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
    count_truncated: &AtomicBool,
) -> Result<Vec<LexicalMatch>, EngineError> {
    let mut lexical_matches = Vec::new();
    let mut searcher = build_searcher(request);
    for result in walker.build() {
        check_cancelled(request)?;
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
        search_file(
            root,
            &path,
            matcher,
            &mut searcher,
            request,
            &mut lexical_matches,
            count_truncated,
        )?;
    }
    Ok(lexical_matches)
}

fn search_paths_parallel(
    root: &Path,
    request: &LexicalSearchRequest,
    matcher: &RegexMatcher,
    walker: &WalkBuilder,
    count_truncated: &AtomicBool,
) -> Result<Vec<LexicalMatch>, EngineError> {
    let lexical_matches = Mutex::new(Vec::new());
    let first_error = Mutex::new(None);
    let stopped = AtomicBool::new(false);
    walker.build_parallel().run(|| {
        let lexical_matches = &lexical_matches;
        let first_error = &first_error;
        let stopped = &stopped;
        let mut searcher = build_searcher(request);
        Box::new(move |result| {
            if stopped.load(Ordering::Acquire)
                || request
                    .signal
                    .as_ref()
                    .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
            {
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
            if let Err(error) = search_file(
                root,
                &path,
                matcher,
                &mut searcher,
                request,
                &mut file_matches,
                count_truncated,
            ) {
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

fn build_searcher(request: &LexicalSearchRequest) -> grep::searcher::Searcher {
    let options = &request.options.matching;
    SearcherBuilder::new()
        .multi_line(options.multiline)
        .invert_match(options.invert_match)
        .stop_on_nonmatch(options.stop_on_nonmatch)
        .line_terminator(if options.crlf {
            grep::matcher::LineTerminator::crlf()
        } else {
            grep::matcher::LineTerminator::byte(b'\n')
        })
        .binary_detection(if options.text {
            BinaryDetection::none()
        } else {
            BinaryDetection::quit(b'\0')
        })
        .bom_sniffing(false)
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
        .line_terminator((!request.options.matching.multiline).then_some(b'\n'))
        .case_insensitive(request.options.matching.ignore_case)
        .fixed_strings(request.options.matching.fixed_strings)
        .word(request.options.matching.word_regexp)
        .case_smart(request.options.matching.smart_case)
        .whole_line(request.options.matching.line_regexp)
        .crlf(request.options.matching.crlf)
        .unicode(!request.options.matching.no_unicode)
        .dot_matches_new_line(
            request.options.matching.multiline && request.options.matching.multiline_dotall,
        );
    if let Some(bytes) = request.options.matching.regex_size_limit {
        builder.size_limit(bytes);
    }
    if let Some(bytes) = request.options.matching.dfa_size_limit {
        builder.dfa_size_limit(bytes);
    }
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
        .max_filesize(request.options.max_file_size_bytes)
        .same_file_system(request.options.matching.one_file_system);

    let filter_root = root.to_path_buf();
    walker.filter_entry(move |entry| {
        let path = entry
            .path()
            .strip_prefix(&filter_root)
            .unwrap_or_else(|_| entry.path());
        !is_hard_ignored_path(path)
    });

    let options = &request.options.matching;
    if request.options.no_ignore {
        walker
            .parents(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false);
    } else {
        walker
            .parents(!options.no_ignore_parent)
            .ignore(!options.no_ignore_dot)
            .git_ignore(!options.no_ignore_vcs)
            .git_global(!options.no_ignore_global && !options.no_ignore_vcs)
            .git_exclude(!options.no_ignore_vcs);
        if !options.no_ignore_dot {
            walker.add_custom_ignore_filename(".rgignore");
        }
    }

    for ignore_file in request
        .options
        .ignore_files
        .iter()
        .filter(|_| !options.no_ignore_files)
    {
        let path = resolve_path(root, ignore_file);
        if let Some(error) = walker.add_ignore(&path) {
            return Err(EngineError::storage_failure(format!(
                "ignore file {} could not be loaded: {error}",
                ignore_file.display()
            )));
        }
    }

    if !request.options.globs.is_empty()
        || !request.options.insensitive_globs.is_empty()
        || !options.glob_rules.is_empty()
    {
        let mut overrides = OverrideBuilder::new(root);
        let rules = request
            .options
            .globs
            .iter()
            .map(|glob| (glob, false))
            .chain(
                request
                    .options
                    .insensitive_globs
                    .iter()
                    .map(|glob| (glob, true)),
            )
            .chain(
                options
                    .glob_rules
                    .iter()
                    .map(|rule| (&rule.pattern, rule.case_insensitive)),
            );
        for (glob, insensitive) in rules {
            overrides
                .case_insensitive(insensitive || options.glob_case_insensitive)
                .map_err(|error| {
                    EngineError::invalid_argument(format!("invalid glob case setting: {error}"))
                })?;
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

struct CancellableReader<'a, R> {
    reader: R,
    signal: Option<&'a tokio_util::sync::CancellationToken>,
}

impl<R: io::Read> io::Read for CancellableReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self
            .signal
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            return Err(io::Error::other("lexical search was cancelled"));
        }
        self.reader.read(buffer)
    }
}

fn search_file(
    root: &Path,
    path: &Path,
    matcher: &RegexMatcher,
    searcher: &mut grep::searcher::Searcher,
    request: &LexicalSearchRequest,
    results: &mut Vec<LexicalMatch>,
    count_truncated: &AtomicBool,
) -> Result<(), EngineError> {
    let absolute_path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let relative_path = absolute_path
        .strip_prefix(root)
        .map_or_else(|_| absolute_path.clone(), Path::to_path_buf);
    let search_result = (|| {
        let mut reader = io::BufReader::new(CancellableReader {
            reader: std::fs::File::open(&absolute_path)?,
            signal: request.signal.as_ref(),
        });
        let header = reader.fill_buf()?;
        let encoded_unicode = header.starts_with(b"\xff\xfe")
            || header.starts_with(b"\xfe\xff")
            || header.starts_with(b"\x00\x00\xfe\xff");
        let mut decoded = None;
        if encoded_unicode {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            let Some(text) = decode_text(&bytes, true) else {
                return Ok(());
            };
            decoded = Some(text.into_owned());
        } else if header.starts_with(b"\xef\xbb\xbf") {
            reader.consume(3);
        }
        let sink = MatchSink {
            matcher,
            signal: request.signal.as_ref(),
            absolute_path: &absolute_path,
            relative_path: &relative_path,
            results,
            options: &request.options.matching,
            count: 0,
            count_truncated,
        };
        match decoded {
            Some(text) => searcher.search_slice(matcher, text.as_bytes(), sink),
            None => searcher.search_reader(matcher, reader, sink),
        }
    })();
    check_cancelled(request)?;
    if let Err(error) = search_result {
        return Err(EngineError::from_io(
            format!("failed to search {}", absolute_path.display()),
            &error,
        ));
    }
    Ok(())
}

struct MatchSink<'a> {
    signal: Option<&'a tokio_util::sync::CancellationToken>,
    matcher: &'a RegexMatcher,
    absolute_path: &'a Path,
    relative_path: &'a Path,
    results: &'a mut Vec<LexicalMatch>,
    options: &'a crate::api::context::options::RgOptions,
    count: usize,
    count_truncated: &'a AtomicBool,
}

impl Sink for MatchSink<'_> {
    type Error = io::Error;

    fn matched(
        &mut self,
        searcher: &grep::searcher::Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self
            .signal
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            return Ok(false);
        }
        let bytes = matched.bytes();
        let line_number = matched
            .line_number()
            .and_then(|line| usize::try_from(line).ok())
            .ok_or_else(|| io::Error::other("search match has no representable line number"))?;
        let line_offset =
            usize::try_from(matched.absolute_byte_offset()).map_err(io::Error::other)?;
        if self.options.invert_match {
            let mut offset = 0;
            for (line, bytes) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
                let selection = grep::matcher::Match::new(0, trim_line_terminator(bytes).len());
                let text = std::str::from_utf8(bytes).ok();
                let line_starts = text.map(line_byte_offsets).unwrap_or_default();
                if !self.record(
                    text.map(|text| (text, line_starts.as_slice())),
                    line_number + line,
                    line_offset + offset,
                    selection,
                )? {
                    return Ok(false);
                }
                offset += bytes.len();
            }
            return Ok(true);
        }
        let mut selections: Vec<grep::matcher::Match> = Vec::new();
        let multiline = searcher.multi_line_with_matcher(self.matcher);
        self.matcher
            .find_iter(bytes, |found| {
                // Multiline matching may report several matches on the same line.
                // Keep one source span per overlapping group of matching lines,
                // so max-count has the same line-based meaning as ripgrep.
                if let Some(previous) = selections.last_mut() {
                    let previous_end = previous.end().saturating_sub(usize::from(
                        previous.end() > previous.start() && bytes[previous.end() - 1] == b'\n',
                    ));
                    if found.start() <= previous_end
                        || !bytes[previous_end..found.start()].contains(&b'\n')
                    {
                        *previous = grep::matcher::Match::new(previous.start(), found.end());
                        return true;
                    }
                }
                selections.push(found);
                multiline
                    && self.options.max_count.is_none_or(|maximum| {
                        selections.len() <= maximum.saturating_sub(self.count)
                    })
            })
            .map_err(io::Error::other)?;
        let text = std::str::from_utf8(bytes).ok();
        let line_starts = text.map(line_byte_offsets).unwrap_or_default();
        for selection in selections {
            if !self.record(
                text.map(|text| (text, line_starts.as_slice())),
                line_number,
                line_offset,
                selection,
            )? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl MatchSink<'_> {
    fn record(
        &mut self,
        text: Option<(&str, &[usize])>,
        line_number: usize,
        line_offset: usize,
        first: grep::matcher::Match,
    ) -> io::Result<bool> {
        if self
            .options
            .max_count
            .is_some_and(|maximum| self.count >= maximum)
        {
            self.count_truncated.store(true, Ordering::Relaxed);
            return Ok(false);
        }
        let Some((text, line_starts)) = text else {
            return Ok(true);
        };
        let Ok(local) =
            crate::utils::text_range_from_offsets(text, line_starts, first.start(), first.end())
        else {
            return Ok(true);
        };
        let map_range = |local: TextRange| {
            let end_byte_offset = line_offset
                .checked_add(local.end_byte_offset())
                .ok_or_else(|| io::Error::other("search match byte offset exceeds usize"))?;
            let end_line = line_number
                .checked_add(local.end_line() - 1)
                .ok_or_else(|| io::Error::other("search match line number exceeds usize"))?;
            TextRange::from_coordinates(
                line_offset + local.start_byte_offset(),
                end_byte_offset,
                line_number + (local.start_line() - 1),
                end_line,
                local.start_byte_column(),
                local.end_byte_column(),
            )
            .map_err(io::Error::other)
        };
        let range = map_range(local)?;
        let bytes = text.as_bytes();
        let content_start = line_starts[local.start_line() - 1];
        let content_end = if first.end() > first.start() && bytes[first.end() - 1] == b'\n' {
            first.end()
        } else {
            line_starts
                .get(local.end_line())
                .copied()
                .unwrap_or(bytes.len())
        };
        let content_end =
            content_start + trim_line_terminator(&bytes[content_start..content_end]).len();
        let content_range = map_range(
            crate::utils::text_range_from_offsets(text, line_starts, content_start, content_end)
                .map_err(io::Error::other)?,
        )?;
        self.count += 1;
        self.results.push(LexicalMatch {
            rank: 0,
            absolute_path: self.absolute_path.to_path_buf(),
            relative_path: self.relative_path.to_path_buf(),
            range,
            excerpt_range: None,
            content_range,
            content: text[content_start..content_end].to_owned(),
        });
        Ok(true)
    }
}

fn trim_line_terminator(bytes: &[u8]) -> &[u8] {
    if let Some(stripped) = bytes.strip_suffix(b"\n") {
        stripped.strip_suffix(b"\r").unwrap_or(stripped)
    } else {
        bytes
    }
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

fn matching_diagnostic_args(options: &crate::api::context::options::RgOptions) -> Vec<String> {
    let mut args = Vec::new();
    for (enabled, name) in [
        (options.smart_case, "--smart-case"),
        (options.line_regexp, "--line-regexp"),
        (options.invert_match, "--invert-match"),
        (options.multiline, "--multiline"),
        (options.multiline_dotall, "--multiline-dotall"),
        (options.crlf, "--crlf"),
        (options.text, "--text"),
        (options.no_unicode, "--no-unicode"),
        (options.stop_on_nonmatch, "--stop-on-nonmatch"),
        (options.no_ignore_dot, "--no-ignore-dot"),
        (options.no_ignore_files, "--no-ignore-files"),
        (options.no_ignore_global, "--no-ignore-global"),
        (options.no_ignore_parent, "--no-ignore-parent"),
        (options.no_ignore_vcs, "--no-ignore-vcs"),
        (options.one_file_system, "--one-file-system"),
        (options.glob_case_insensitive, "--glob-case-insensitive"),
    ] {
        push_diagnostic_switch(&mut args, enabled, name);
    }
    for (value, name) in [
        (options.max_count, "--max-count"),
        (options.threads, "--threads"),
        (options.regex_size_limit, "--regex-size-limit"),
        (options.dfa_size_limit, "--dfa-size-limit"),
    ] {
        push_diagnostic_value(&mut args, name, value.map(|value| value.to_string()));
    }
    args
}

fn diagnostic_args(request: &LexicalSearchRequest) -> Vec<String> {
    let options = &request.options.matching;
    let mut args = matching_diagnostic_args(options);
    push_diagnostic_switch(
        &mut args,
        request.options.matching.fixed_strings,
        "--fixed-strings",
    );
    push_diagnostic_switch(
        &mut args,
        request.options.matching.ignore_case,
        "--ignore-case",
    );
    push_diagnostic_switch(
        &mut args,
        request.options.matching.word_regexp,
        "--word-regexp",
    );
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
    for glob in &request.options.insensitive_globs {
        args.extend(["--iglob".to_owned(), glob.clone()]);
    }
    for rule in &options.glob_rules {
        args.extend([
            if rule.case_insensitive {
                "--iglob"
            } else {
                "--glob"
            }
            .to_owned(),
            rule.pattern.clone(),
        ]);
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

struct ContextSource {
    text: String,
    line_starts: Vec<usize>,
}

impl ContextSource {
    fn read(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let text = decode_text(&bytes, true)?.into_owned();
        let line_starts = line_byte_offsets(&text);
        Some(Self { text, line_starts })
    }

    fn line_end(&self, line: usize) -> usize {
        self.line_starts
            .get(line)
            .copied()
            .unwrap_or(self.text.len())
    }
}

fn expand_context(matches: &mut [LexicalMatch], request: &LexicalSearchRequest) {
    let before = request.options.matching.before_context;
    let after = request.options.matching.after_context;
    if before == 0 && after == 0 {
        return;
    }

    let mut cache: HashMap<PathBuf, Option<ContextSource>> = HashMap::new();
    for item in matches {
        let source = cache
            .entry(item.absolute_path.clone())
            .or_insert_with(|| ContextSource::read(&item.absolute_path));
        let Some(source) = source else {
            continue;
        };
        let excerpt = item.range;
        if crate::utils::text_range_from_offsets(
            &source.text,
            &source.line_starts,
            excerpt.start_byte_offset(),
            excerpt.end_byte_offset(),
        )
        .ok()
            != Some(excerpt)
        {
            continue;
        }
        let last_matched_line = if excerpt.end_byte_column() == 0
            && excerpt.end_byte_offset() > excerpt.start_byte_offset()
        {
            excerpt.end_line() - 1
        } else {
            excerpt.end_line()
        };
        let matched_start = source.line_starts[excerpt.start_line() - 1];
        let matched_end = source.line_end(last_matched_line);
        let matched_lines = &source.text[matched_start..matched_end];
        if trim_line_terminator(matched_lines.as_bytes()) != item.content.as_bytes() {
            continue;
        }

        let start_line = excerpt.start_line().saturating_sub(before).max(1);
        let line_count = source.line_starts.len() - usize::from(source.text.ends_with('\n'));
        let end_line = last_matched_line
            .saturating_add(after)
            .min(line_count.max(last_matched_line));
        let last_start = source.line_starts[end_line - 1];
        let last_line = &source.text[last_start..source.line_end(end_line)];
        let start_byte_offset = source.line_starts[start_line - 1];
        let end_byte_offset = (last_start + trim_line_terminator(last_line.as_bytes()).len())
            .max(excerpt.end_byte_offset());
        let Ok(range) = crate::utils::text_range_from_offsets(
            &source.text,
            &source.line_starts,
            start_byte_offset,
            end_byte_offset,
        ) else {
            continue;
        };
        let content = source.text[start_byte_offset..end_byte_offset].to_owned();
        item.excerpt_range = Some(excerpt);
        item.range = range;
        item.content_range = range;
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use super::types::{LexicalMatch, LexicalOptions, LexicalSearchReply, LexicalSearchRequest};
    use tempfile::TempDir;

    use super::{
        DEFAULT_MAX_SEARCH_THREADS, LexicalSearchService, TextRange, default_worker_threads,
        expand_context, worker_threads_for_search,
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
    async fn searches_in_process_and_reports_utf8_byte_columns() {
        let root = TempDir::new().expect("temp dir");
        std::fs::write(
            root.path().join("a.txt"),
            "before\nlet x = \"😀你好\";\nafter\n",
        )
        .expect("fixture");
        let reply = search(&LexicalSearchService::new(), root.path(), &request("你好")).await;

        assert_eq!(reply.diagnostics.backend, "grep");
        assert_eq!(reply.matches.len(), 1);
        assert_eq!(reply.matches[0].relative_path, Path::new("a.txt"));
        let range = reply.matches[0].range;
        assert_eq!(range.start_line(), 2);
        assert_eq!(range.start_byte_column(), 13);
        assert_eq!(range.end_byte_column(), 19);
        assert_eq!(range.start_byte_offset(), 20);
        assert_eq!(range.end_byte_offset(), 26);
    }

    #[tokio::test]
    async fn multiline_matches_preserve_crlf_and_half_open_coordinates() {
        let root = TempDir::new().expect("temp dir");
        let text = "before\r\n中😀\r\n尾\r\nafter\r\n";
        fs::write(root.path().join("a.txt"), text).expect("fixture");
        let mut request = request("中😀\\r\\n|尾");
        request.options.matching.multiline = true;
        let reply = search(&LexicalSearchService::new(), root.path(), &request).await;
        assert_eq!(reply.matches.len(), 2);
        let start = text.find('中').expect("first match");
        let end = text.find('尾').expect("second match");
        assert_eq!(
            reply.matches[0].range,
            TextRange::from_coordinates(start, end, 2, 3, 0, 0).expect("half-open range")
        );
        assert_eq!(reply.matches[0].content, "中😀");
        assert_eq!(
            reply.matches[1].range,
            TextRange::from_coordinates(end, end + '尾'.len_utf8(), 3, 3, 0, 3)
                .expect("next line range")
        );
        assert_eq!(reply.matches[1].content, "尾");
    }

    #[tokio::test]
    async fn byte_matches_skip_partial_characters_without_losing_later_lines() {
        let root = TempDir::new().expect("temp dir");
        let text = "中😀\r\nneedle\r\n";
        fs::write(root.path().join("a.txt"), text).expect("fixture");
        let mut request = request("\\xE4|needle");
        request.options.matching.no_unicode = true;
        let reply = search(&LexicalSearchService::new(), root.path(), &request).await;
        assert_eq!(reply.matches.len(), 1);
        let start = text.find("needle").expect("complete match");
        assert_eq!(
            reply.matches[0].range,
            TextRange::from_coordinates(start, start + 6, 2, 2, 0, 6).expect("complete range")
        );
        assert_eq!(reply.matches[0].content, "needle");
    }

    #[tokio::test]
    async fn inverted_matches_keep_valid_lines_and_honor_count_before_invalid_utf8() {
        let root = TempDir::new().expect("temp dir");
        fs::write(
            root.path().join("a.txt"),
            b"skip\r\nfirst\r\n\xff\r\nlast\r\n",
        )
        .expect("fixture");
        let mut request = request("skip");
        request.options.matching.invert_match = true;
        request.options.matching.crlf = true;
        let service = LexicalSearchService::new();
        let reply = search(&service, root.path(), &request).await;
        assert_eq!(reply.matches.len(), 2);
        for (item, (start, end, line, content)) in reply
            .matches
            .iter()
            .zip([(6, 11, 2, "first"), (16, 20, 4, "last")])
        {
            assert_eq!(
                item.range,
                TextRange::from_coordinates(start, end, line, line, 0, content.len())
                    .expect("valid line range")
            );
            assert_eq!(item.content, content);
        }
        request.options.matching.max_count = Some(1);
        let limited = search(&service, root.path(), &request).await;
        assert_eq!(limited.matches.len(), 1);
        assert_eq!(limited.matches[0].content, "first");
        assert!(limited.diagnostics.truncated);
    }

    #[tokio::test]
    async fn decodes_unicode_sources_strictly_and_preserves_context_line_endings() {
        let root = TempDir::new().expect("temp dir");
        let text = "前😀\r\nlet x = \"😀你好\";\r\n尾巴\r\n";
        let fixtures = [
            [b"\xef\xbb\xbf".as_slice(), text.as_bytes()].concat(),
            [
                b"\xff\xfe".as_slice(),
                &text
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>(),
            ]
            .concat(),
            [
                b"\xfe\xff".as_slice(),
                &text
                    .encode_utf16()
                    .flat_map(u16::to_be_bytes)
                    .collect::<Vec<_>>(),
            ]
            .concat(),
            [
                b"\xff\xfe\0\0".as_slice(),
                &text
                    .chars()
                    .flat_map(|character| u32::from(character).to_le_bytes())
                    .collect::<Vec<_>>(),
            ]
            .concat(),
            [
                b"\0\0\xfe\xff".as_slice(),
                &text
                    .chars()
                    .flat_map(|character| u32::from(character).to_be_bytes())
                    .collect::<Vec<_>>(),
            ]
            .concat(),
        ];
        for (index, bytes) in fixtures.iter().enumerate() {
            fs::write(root.path().join(format!("source-{index}.txt")), bytes).expect("fixture");
        }
        fs::write(
            root.path().join("invalid.txt"),
            b"\xff\xfe\x60\x4f\x7d\x59\x00\xd8",
        )
        .expect("invalid UTF-16 fixture");
        let mut request = request("你好");
        request.options.matching.before_context = 1;
        request.options.matching.after_context = 1;
        let reply = search(&LexicalSearchService::new(), root.path(), &request).await;
        assert_eq!(reply.matches.len(), fixtures.len());
        for item in reply.matches {
            assert_eq!(
                item.content,
                text.strip_suffix("\r\n").expect("final newline")
            );
            assert_eq!(item.range.start_line(), 1);
            assert_eq!(item.range.end_line(), 3);
            assert_eq!(item.range.end_byte_column(), "尾巴".len());
            assert_eq!(item.content_range, item.range);
            assert_eq!(
                crate::utils::slice_text(
                    text,
                    item.range.start_byte_offset(),
                    item.range.end_byte_offset()
                )
                .expect("context range"),
                item.content
            );
            let excerpt = item.excerpt_range.expect("matched span");
            assert_eq!(excerpt.start_line(), 2);
            assert_eq!(excerpt.start_byte_column(), 13);
            assert_eq!(excerpt.end_byte_column(), 19);
            assert_eq!(
                crate::utils::slice_text(
                    text,
                    excerpt.start_byte_offset(),
                    excerpt.end_byte_offset()
                )
                .expect("matched range"),
                "你好"
            );
            let line = text.lines().nth(1).expect("matched line");
            assert_eq!(
                &line[excerpt.start_byte_column()..excerpt.end_byte_column()],
                "你好"
            );
        }
    }

    #[test]
    fn context_preserves_a_half_open_match_ending_at_the_next_line() {
        let root = TempDir::new().expect("temp dir");
        let text = "前😀\r\nlet x = \"😀你好\";\r\n尾巴\r\n";
        fs::write(root.path().join("source-0.txt"), text).expect("fixture");
        let mut request = request("你好");
        request.options.matching.before_context = 1;
        let start = text.find("let").expect("matched line");
        let end = text.find("尾巴").expect("following line");
        let range =
            TextRange::from_coordinates(start, end, 2, 3, 0, 0).expect("span including newline");
        let mut items = [LexicalMatch {
            rank: 1,
            absolute_path: root.path().join("source-0.txt"),
            relative_path: "source-0.txt".into(),
            range,
            excerpt_range: None,
            content_range: crate::utils::text_range_from_offsets(
                text,
                &crate::utils::line_byte_offsets(text),
                start,
                start + text[start..end].trim_end_matches("\r\n").len(),
            )
            .expect("visible source range"),
            content: text[start..end].trim_end_matches("\r\n").to_owned(),
        }];
        request.options.matching.after_context = 0;
        expand_context(&mut items, &request);
        assert_eq!(items[0].content, &text[..end]);
        assert_eq!(items[0].content_range, items[0].range);
        assert_eq!(items[0].excerpt_range, Some(range));
        assert_eq!(items[0].range.end_line(), 3);
        assert_eq!(items[0].range.end_byte_column(), 0);
        assert_eq!(
            crate::utils::slice_text(
                text,
                items[0].range.start_byte_offset(),
                items[0].range.end_byte_offset(),
            )
            .expect("context range"),
            items[0].content
        );
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
            matching: crate::api::context::options::RgOptions {
                fixed_strings: true,
                ignore_case: true,
                word_regexp: true,
                before_context: 1,
                after_context: 1,
                ..Default::default()
            },
            ..LexicalOptions::default()
        };

        let reply = search(&LexicalSearchService::new(), root.path(), &request).await;
        assert_eq!(reply.matches.len(), 1);
        assert!(reply.diagnostics.truncated);
        assert_eq!(reply.matches[0].range.start_line(), 1);
        assert_eq!(
            reply.matches[0]
                .excerpt_range
                .as_ref()
                .expect("excerpt")
                .start_line(),
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
        assert_eq!(reply.matches[0].range.start_line(), 2);
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

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[test]
    fn cancelled_reader_stops_before_reading_more_source() {
        let signal = tokio_util::sync::CancellationToken::new();
        let mut reader = CancellableReader {
            reader: io::Cursor::new(b"source"),
            signal: Some(&signal),
        };
        let mut buffer = [0; 2];
        assert_eq!(reader.read(&mut buffer).expect("initial read"), 2);
        signal.cancel();
        assert!(reader.read(&mut buffer).is_err());
        assert_eq!(reader.reader.position(), 2);
    }

    #[tokio::test]
    async fn cancelled_search_does_not_wait_for_admission() {
        let service = LexicalSearchService::new().with_max_searches(1);
        let _slot = service.search_slots.acquire().await.expect("occupied slot");
        let signal = tokio_util::sync::CancellationToken::new();
        let request = LexicalSearchRequest {
            patterns: vec!["needle".into()],
            signal: Some(signal.clone()),
            ..LexicalSearchRequest::default()
        };
        let search = service.search(Path::new("."), &request);
        let cancel = async {
            tokio::task::yield_now().await;
            signal.cancel();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(search, cancel)
        })
        .await
        .expect("cancel releases waiter");
        assert_eq!(
            result.expect_err("cancelled").code(),
            EngineError::CANCELLED
        );
    }
}
