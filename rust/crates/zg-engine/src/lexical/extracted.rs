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
        count: &mut usize,
    ) -> Result<Vec<LexicalMatch>, EngineError> {
        check_cancelled(&self.request)?;
        let mut collected = LexicalMatchBuilder::default();
        let mut sink = MatchSink {
            matcher: &self.matcher,
            signal: self.request.signal.as_ref(),
            results: &mut collected,
            options: &self.request.options.matching,
            count: *count,
            count_truncated: &self.truncated,
        };
        build_searcher(&self.request)
            .search_slice(&self.matcher, text.as_bytes(), &mut sink)
            .map_err(|e| EngineError::from_io("search extracted PDF text", &e))?;
        *count = sink.count;
        check_cancelled(&self.request)?;
        let mut matches = Vec::new();
        collected.append_to(PathBuf::new(), PathBuf::new(), &mut matches);
        if self.request.options.matching.stop_on_nonmatch && !matches.is_empty() {
            self.truncated.store(true, Ordering::Relaxed);
        }
        Ok(matches)
    }
}
