//! Reuse the embedded grep matcher and sink for extracted entity text.
use super::{
    LexicalMatch, LexicalMatchBuilder, LexicalSearchRequest, MatchSink, RegexMatcher,
    build_matcher, build_searcher, check_cancelled, load_patterns,
};
use crate::EngineError;
use std::{
    collections::BTreeSet,
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
    seen: BTreeSet<u64>,
    searched_end: u64,
    pub(crate) stopped: bool,
}

/// Per-entity observations, with matching-line identity in the source document.
pub(super) struct SourceMatches<'a> {
    pub offset: u64,
    pub seen: &'a mut BTreeSet<u64>,
    pub first: Option<u64>,
    pub last_line: Option<usize>,
}

impl SourceMatches<'_> {
    pub fn observe(&mut self, range: crate::domain::TextRange) -> std::io::Result<bool> {
        let start = self
            .offset
            .checked_add(u64::try_from(range.start_byte_offset()).map_err(std::io::Error::other)?)
            .ok_or_else(|| std::io::Error::other("Extracted source offset overflow"))?;
        self.first.get_or_insert(start);
        self.last_line = Some(range.end_line());
        Ok(self.seen.insert(start))
    }
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
        source_offset: u64,
        state: &mut ExtractedFileState,
    ) -> Result<Vec<LexicalMatch>, EngineError> {
        check_cancelled(&self.request)?;
        if state.stopped {
            return Ok(Vec::new());
        }
        let mut positions = SourceMatches {
            offset: source_offset,
            seen: &mut state.seen,
            first: None,
            last_line: None,
        };
        let mut collected = LexicalMatchBuilder::default();
        let mut sink = MatchSink {
            matcher: &self.matcher,
            signal: self.request.signal.as_ref(),
            results: &mut collected,
            options: &self.request.options.matching,
            count: state.count,
            source_matches: Some(&mut positions),
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
                && positions
                    .first
                    .is_none_or(|start| start > state.searched_end.max(source_offset))
            {
                state.stopped = true;
                return Ok(Vec::new());
            }
            if let Some(last_line) = positions.last_line {
                state.stopped = last_line < text.lines().count();
                self.truncated.store(true, Ordering::Relaxed);
            }
        }
        state.searched_end = state
            .searched_end
            .max(source_offset.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX)));
        state.count = count;
        Ok(matches)
    }
}

#[cfg(test)]
mod tests;
