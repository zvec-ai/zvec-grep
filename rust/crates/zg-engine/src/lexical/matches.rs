use std::{
    fmt,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::domain::TextRange;

struct LexicalFile {
    absolute_path: PathBuf,
    relative_path: PathBuf,
}

/// Matches share file paths and immutable source text until API conversion.
#[derive(Clone)]
pub(crate) struct LexicalMatch {
    pub rank: usize,
    pub range: TextRange,
    pub excerpt_range: Option<TextRange>,
    /// Source coordinates of the returned lines, which may extend beyond the match.
    pub content_range: TextRange,
    pub match_ranges: Vec<TextRange>,
    file: Arc<LexicalFile>,
    // Keep the String allocation movable for the common single-hit file.
    text: Arc<String>,
    content_span: Range<usize>,
}

impl LexicalMatch {
    pub(crate) fn absolute_path(&self) -> &Path {
        &self.file.absolute_path
    }

    pub(crate) fn relative_path(&self) -> &Path {
        &self.file.relative_path
    }

    pub(crate) fn content(&self) -> &str {
        &self.text[self.content_span.clone()]
    }

    pub(super) fn set_content(&mut self, text: Arc<String>, span: Range<usize>) {
        self.text = text;
        self.content_span = span;
    }

    /// Materialize owned API fields only after sorting, truncation, and deduplication.
    pub(crate) fn into_paths_and_content(self) -> (PathBuf, PathBuf, String) {
        let (absolute_path, relative_path) = match Arc::try_unwrap(self.file) {
            Ok(file) => (file.absolute_path, file.relative_path),
            Err(file) => (file.absolute_path.clone(), file.relative_path.clone()),
        };
        let content = match Arc::try_unwrap(self.text) {
            Ok(text) if self.content_span == (0..text.len()) => text,
            Ok(text) => text[self.content_span].to_owned(),
            Err(text) => text[self.content_span].to_owned(),
        };
        (absolute_path, relative_path, content)
    }

    #[cfg(test)]
    pub(crate) fn new(
        absolute_path: PathBuf,
        relative_path: PathBuf,
        range: TextRange,
        content_range: TextRange,
        content: String,
    ) -> Self {
        Self {
            rank: 0,
            range,
            excerpt_range: None,
            content_range,
            match_ranges: Vec::new(),
            file: Arc::new(LexicalFile {
                absolute_path,
                relative_path,
            }),
            content_span: 0..content.len(),
            text: Arc::new(content),
        }
    }
}

impl PartialEq for LexicalMatch {
    fn eq(&self, other: &Self) -> bool {
        self.rank == other.rank
            && self.absolute_path() == other.absolute_path()
            && self.relative_path() == other.relative_path()
            && self.range == other.range
            && self.excerpt_range == other.excerpt_range
            && self.content_range == other.content_range
            && self.match_ranges == other.match_ranges
            && self.content() == other.content()
    }
}

impl Eq for LexicalMatch {}

impl fmt::Debug for LexicalMatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LexicalMatch")
            .field("rank", &self.rank)
            .field("absolute_path", &self.absolute_path())
            .field("relative_path", &self.relative_path())
            .field("range", &self.range)
            .field("excerpt_range", &self.excerpt_range)
            .field("content_range", &self.content_range)
            .field("match_ranges", &self.match_ranges)
            .field("content", &self.content())
            .finish_non_exhaustive()
    }
}

struct PendingMatch {
    range: TextRange,
    content_range: TextRange,
    match_ranges: Vec<TextRange>,
    content_span: Range<usize>,
}

#[derive(Default)]
pub(super) struct LexicalMatchBuilder {
    text: String,
    matches: Vec<PendingMatch>,
}

impl LexicalMatchBuilder {
    pub(super) fn push(
        &mut self,
        range: TextRange,
        content_range: TextRange,
        match_ranges: Vec<TextRange>,
        content: &str,
    ) {
        let start = self.text.len();
        self.text.push_str(content);
        self.matches.push(PendingMatch {
            range,
            content_range,
            match_ranges,
            content_span: start..self.text.len(),
        });
    }

    pub(super) fn append_to(
        self,
        absolute_path: PathBuf,
        relative_path: PathBuf,
        results: &mut Vec<LexicalMatch>,
    ) {
        if self.matches.is_empty() {
            return;
        }
        let file = Arc::new(LexicalFile {
            absolute_path,
            relative_path,
        });
        let text = Arc::new(self.text);
        results.extend(self.matches.into_iter().map(|matched| LexicalMatch {
            rank: 0,
            file: Arc::clone(&file),
            range: matched.range,
            excerpt_range: None,
            content_range: matched.content_range,
            match_ranges: matched.match_ranges,
            text: Arc::clone(&text),
            content_span: matched.content_span,
        }));
    }
}
