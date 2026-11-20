//! Reuse the embedded grep matcher and sink for extracted entity text.
use super::{
    LexicalMatch, LexicalMatchBuilder, LexicalSearchRequest, MatchSink, RegexMatcher,
    build_matcher, build_searcher, check_cancelled, load_patterns,
};
use crate::EngineError;
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

pub(crate) struct ExtractedMatcher {
    matcher: RegexMatcher,
    request: LexicalSearchRequest,
    pub(crate) truncated: AtomicBool,
}

/// Search limits and stop-on-nonmatch state belong to the source file, even
/// when its searchable text is stored in several independently matched entities.
#[derive(Default)]
pub(crate) struct ExtractedFileState {
    count: usize,
    pub(crate) stopped: bool,
}

impl ExtractedMatcher {
    pub(crate) fn new(root: &Path, request: &LexicalSearchRequest) -> Result<Self, EngineError> {
        let patterns = load_patterns(root, request)?;
        Ok(Self {
            matcher: build_matcher(&patterns, request)?,
            request: request.clone(),
            truncated: AtomicBool::new(false),
        })
    }
    pub(crate) fn search(
        &self,
        text: &str,
        state: &mut ExtractedFileState,
    ) -> Result<Vec<LexicalMatch>, EngineError> {
        check_cancelled(&self.request)?;
        if state.stopped {
            return Ok(Vec::new());
        }
        let mut collected = LexicalMatchBuilder::default();
        let mut sink = MatchSink {
            matcher: &self.matcher,
            signal: self.request.signal.as_ref(),
            results: &mut collected,
            options: &self.request.options.matching,
            count: state.count,
            count_truncated: &self.truncated,
        };
        build_searcher(&self.request)
            .search_slice(&self.matcher, text.as_bytes(), &mut sink)
            .map_err(|e| EngineError::from_io("search extracted PDF text", &e))?;
        let count = sink.count;
        check_cancelled(&self.request)?;
        let mut matches = Vec::new();
        collected.append_to(PathBuf::new(), PathBuf::new(), &mut matches);
        if self.request.options.matching.stop_on_nonmatch {
            // The grep searcher handles gaps inside an entity. Carry its file
            // semantics across boundaries: after a previous match, this entity
            // must start with a selected line, and a trailing gap stops the file.
            if state.count > 0
                && !text.is_empty()
                && matches
                    .first()
                    .is_none_or(|m| m.content_range.start_line() != 1)
            {
                state.stopped = true;
                return Ok(Vec::new());
            }
            if let Some(last) = matches.last() {
                state.stopped = last.content_range.end_line() < text.lines().count();
                self.truncated.store(true, Ordering::Relaxed);
            }
        }
        state.count = count;
        Ok(matches)
    }
}

#[cfg(test)]
mod tests;
