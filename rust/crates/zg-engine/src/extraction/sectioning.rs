//! Format-independent heading hierarchy and section boundaries.
//!
//! Offsets refer only to the supplied UTF-8 text. Format adapters own heading
//! recognition, metadata, and mapping extracted text back to the original file.

use std::ops::Range;

use crate::{EngineError, EngineResult};

/// A recognized heading and the separator immediately before it.
#[derive(Debug)]
pub(super) struct Heading {
    pub title: String,
    /// Positive relative depth; no format-specific upper bound is imposed.
    pub level: usize,
    /// Start of this heading's section in the supplied text.
    pub start: usize,
    /// End of the preceding section. Use `start` to retain all separators, or
    /// an earlier offset to exclude whitespace without rewriting either section.
    pub previous_end: usize,
}

/// A disjoint section, with ancestors ordered from outermost to innermost.
/// Ancestor content is not duplicated into child sections or vice versa.
#[derive(Debug)]
pub(super) struct Section<'heading> {
    pub range: Range<usize>,
    pub heading: Option<&'heading Heading>,
    pub ancestors: Vec<&'heading str>,
}

/// Splits at every heading, regardless of its level, preserving source order.
/// A nonblank preamble becomes an untitled section. No headings returns no
/// sections so the adapter can choose its existing unstructured-text fallback.
///
/// Headings must have positive levels and strictly increasing UTF-8 offsets.
/// Separators may contain only whitespace and must not consume a preceding
/// heading's entire section. Invalid input returns an error before any slicing.
pub(super) fn split_sections<'heading>(
    text: &str,
    headings: &'heading [Heading],
) -> EngineResult<Vec<Section<'heading>>> {
    validate_headings(text, headings)?;
    let Some(first) = headings.first() else {
        return Ok(Vec::new());
    };

    let mut sections = Vec::new();
    if !text[..first.previous_end].trim().is_empty() {
        sections.push(Section {
            range: 0..first.previous_end,
            heading: None,
            ancestors: Vec::new(),
        });
    }

    let mut stack: Vec<&Heading> = Vec::new();
    for (index, heading) in headings.iter().enumerate() {
        while stack
            .last()
            .is_some_and(|parent| parent.level >= heading.level)
        {
            stack.pop();
        }
        let end = headings
            .get(index + 1)
            .map_or(text.len(), |next| next.previous_end);
        sections.push(Section {
            range: heading.start..end,
            heading: Some(heading),
            ancestors: stack.iter().map(|parent| parent.title.as_str()).collect(),
        });
        stack.push(heading);
    }
    Ok(sections)
}

fn validate_headings(text: &str, headings: &[Heading]) -> EngineResult<()> {
    let mut previous_start = None;
    for (index, heading) in headings.iter().enumerate() {
        if heading.level == 0
            || heading.start >= text.len()
            || previous_start.is_some_and(|start| heading.previous_end <= start)
            || !text
                .get(heading.previous_end..heading.start)
                .is_some_and(|separator| separator.trim().is_empty())
        {
            return Err(EngineError::invalid_argument(format!(
                "invalid heading {index}: expected a positive level, ordered UTF-8 offsets within the text, and a whitespace-only separator"
            )));
        }
        previous_start = Some(heading.start);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heading(title: &str, level: usize, start: usize, previous_end: usize) -> Heading {
        Heading {
            title: title.to_owned(),
            level,
            start,
            previous_end,
        }
    }

    #[test]
    fn supports_inline_headings_repeated_titles_and_unbounded_depth() {
        // The adapter supplies recognized titles; the splitter does not parse
        // commands or assume that a heading starts on a new line.
        let text = "前言 😀 \\section{Same}a\\subsection{Same}b\\section{Next}c";
        let first = text.find("\\section{Same}").expect("first heading");
        let child = text.find("\\subsection").expect("child heading");
        let next = text.find("\\section{Next}").expect("next heading");
        let headings = [
            heading("Same", 1, first, first),
            heading("Same", 20, child, child),
            heading("Next", 1, next, next),
        ];
        let sections = split_sections(text, &headings).expect("sections");
        assert_eq!(
            sections
                .iter()
                .map(|section| &text[section.range.clone()])
                .collect::<Vec<_>>(),
            [
                "前言 😀 ",
                "\\section{Same}a",
                "\\subsection{Same}b",
                "\\section{Next}c"
            ]
        );
        assert!(sections[0].heading.is_none());
        assert_eq!(sections[2].ancestors, ["Same"]);
        assert!(sections[3].ancestors.is_empty());
        let matched = sections[2].heading.expect("heading");
        assert_eq!(matched.title, "Same");
        assert_eq!(matched.start, child);
    }

    #[test]
    fn restores_ancestors_after_skipped_levels_and_siblings() {
        let text = "A\nB\nC\nD\nE\n";
        let headings = [
            heading("A", 1, 0, 0),
            heading("B", 4, 2, 2),
            heading("C", 9, 4, 4),
            heading("D", 4, 6, 6),
            heading("E", 2, 8, 8),
        ];
        let sections = split_sections(text, &headings).expect("sections");
        let ancestors = sections
            .iter()
            .map(|section| section.ancestors.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            ancestors,
            [vec![], vec!["A"], vec!["A", "B"], vec!["A"], vec!["A"]]
        );
        assert_eq!(sections.last().expect("last section").range, 8..text.len());
    }

    #[test]
    fn respects_adapter_separators_without_normalizing_newlines() {
        let text = "前言\r\nTitle\nbody\r\nNext\nend\n";
        let first = text.find("Title").expect("first heading");
        let next = text.find("Next").expect("next heading");
        let headings = [
            heading("Title", 1, first, first - 1),
            heading("Next", 1, next, next - 1),
        ];
        let sections = split_sections(text, &headings).expect("sections");
        assert_eq!(
            sections
                .iter()
                .map(|section| &text[section.range.clone()])
                .collect::<Vec<_>>(),
            ["前言\r", "Title\nbody\r", "Next\nend\n"]
        );
    }

    #[test]
    fn leaves_unstructured_fallback_to_the_adapter_and_skips_blank_preambles() {
        for text in ["", " \r\n", "plain text"] {
            assert!(split_sections(text, &[]).expect("no headings").is_empty());
        }
        let text = " \r\nTitle";
        let headings = [heading("Title", 1, 3, 3)];
        let sections = split_sections(text, &headings).expect("sections");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].range, 3..text.len());
    }

    #[test]
    fn rejects_invalid_adapter_offsets_and_non_whitespace_gaps() {
        let text = "😀 title\nnext\n";
        let next = text.find("next").expect("next heading");
        for headings in [
            vec![heading("zero level", 0, 0, 0)],
            vec![heading("inside a character", 1, 1, 1)],
            vec![heading("reversed separator", 1, 0, 1)],
            vec![heading("at EOF", 1, text.len(), text.len())],
            vec![heading("past EOF", 1, text.len() + 1, text.len() + 1)],
            vec![heading("lost preamble", 1, next, 0)],
            vec![heading("first", 1, 0, 0), heading("duplicate", 2, 0, 0)],
            vec![
                heading("first", 1, next, next),
                heading("unordered", 2, 0, 0),
            ],
            vec![
                heading("first", 1, 0, 0),
                heading("lost section", 2, next, 0),
            ],
            vec![
                heading("first", 1, 0, 0),
                heading("inside separator", 2, next, 1),
            ],
        ] {
            assert!(split_sections(text, &headings).is_err(), "{headings:?}");
        }
    }
}
