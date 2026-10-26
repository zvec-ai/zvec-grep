use std::borrow::Cow;

use grep::{
    matcher::Matcher,
    regex::{RegexMatcher, RegexMatcherBuilder},
};

#[derive(Default)]
pub(crate) struct QueryHighlighter {
    matchers: Vec<RegexMatcher>,
}

impl QueryHighlighter {
    pub fn new(query: &str, enabled: bool) -> Self {
        if !enabled {
            return Self::default();
        }
        let query = query.trim();
        let mut terms = Vec::new();
        if query.encode_utf16().count() > 1 {
            terms.push(query);
        }
        // Keep the phrase and token rules aligned with the Node.js CLI.
        let Ok(tokenizer) = RegexMatcher::new(r"[A-Za-z_~][A-Za-z0-9_~]*|[0-9]+(?:\.[0-9]+)?")
        else {
            return Self::default();
        };
        let _ = tokenizer.find_iter(query.as_bytes(), |found| {
            let term = &query[found.start()..found.end()];
            if term.len() >= 2 && !is_ignored_term(term) {
                terms.push(term);
            }
            true
        });
        terms.sort_unstable();
        terms.dedup();
        Self {
            matchers: terms
                .into_iter()
                .filter_map(|term| {
                    RegexMatcherBuilder::new()
                        .fixed_strings(true)
                        .case_insensitive(true)
                        .build(term)
                        .ok()
                })
                .collect(),
        }
    }

    pub fn highlight<'a>(&self, value: &'a str) -> Cow<'a, str> {
        let mut ranges = Vec::new();
        for matcher in &self.matchers {
            if matcher
                .find_iter(value.as_bytes(), |found| {
                    ranges.push((found.start(), found.end()));
                    true
                })
                .is_err()
            {
                return Cow::Borrowed(value);
            }
        }
        highlight_ranges(value, ranges)
    }
}

pub(crate) fn highlight_ranges(
    value: &str,
    ranges: impl IntoIterator<Item = (usize, usize)>,
) -> Cow<'_, str> {
    let mut ranges: Vec<_> = ranges
        .into_iter()
        .map(|(start, end)| (start, end.min(value.len())))
        .filter(|&(start, end)| {
            start < end && value.is_char_boundary(start) && value.is_char_boundary(end)
        })
        .collect();
    if ranges.is_empty() {
        return Cow::Borrowed(value);
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        if let Some(previous) = merged.last_mut()
            && start <= previous.1
        {
            previous.1 = previous.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    let mut output = String::new();
    let mut cursor = 0;
    for (start, end) in merged {
        output.push_str(&value[cursor..start]);
        output.push_str("\x1b[1;33m");
        output.push_str(&value[start..end]);
        output.push_str("\x1b[0m");
        cursor = end;
    }
    output.push_str(&value[cursor..]);
    Cow::Owned(output)
}

fn is_ignored_term(term: &str) -> bool {
    matches!(
        term.to_ascii_lowercase().as_str(),
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "by"
            | "class"
            | "enum"
            | "find"
            | "for"
            | "from"
            | "function"
            | "how"
            | "in"
            | "interface"
            | "is"
            | "method"
            | "of"
            | "on"
            | "or"
            | "struct"
            | "the"
            | "to"
            | "type"
            | "what"
            | "where"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_ranges_merge_and_clip_without_splitting_utf8() {
        assert_eq!(
            highlight_ranges("前needle后", [(6, 9), (3, 8), (0, 1), (9, usize::MAX)]),
            "前\x1b[1;33mneedle后\x1b[0m"
        );
        assert_eq!(
            highlight_ranges("前", [(0, 1), (1, 2), (3, 3), (usize::MAX, usize::MAX)]),
            "前"
        );
    }

    #[test]
    fn query_terms_follow_phrase_token_and_overlap_rules() {
        for (query, source, expected) in [
            (
                "find ImportantSymbol",
                "prefix IMPORTANTsymbol here",
                "prefix \x1b[1;33mIMPORTANTsymbol\x1b[0m here",
            ),
            (
                "find needle",
                "find needle and needle",
                "\x1b[1;33mfind needle\x1b[0m and \x1b[1;33mneedle\x1b[0m",
            ),
            ("abcd cdef", "abcdef", "\x1b[1;33mabcdef\x1b[0m"),
            (
                "needle needles",
                "needlesneedle",
                "\x1b[1;33mneedlesneedle\x1b[0m",
            ),
            (
                "  1.2 src/auth.ts  ",
                "1.2 src/auth.ts then 1.2 AUTH",
                "\x1b[1;33m1.2 src/auth.ts\x1b[0m then \x1b[1;33m1.2\x1b[0m \x1b[1;33mAUTH\x1b[0m",
            ),
            (
                "find [needle]",
                "[needle] needlex",
                "[\x1b[1;33mneedle\x1b[0m] \x1b[1;33mneedle\x1b[0mx",
            ),
            (
                "find the x",
                "find the xylophone",
                "\x1b[1;33mfind the x\x1b[0mylophone",
            ),
            ("find the x", "the find x", "the find x"),
            ("x", "x", "x"),
            ("", "needle", "needle"),
            ("  ", "needle", "needle"),
        ] {
            assert_eq!(
                QueryHighlighter::new(query, true).highlight(source),
                expected,
                "{query}"
            );
            assert_eq!(
                QueryHighlighter::new(query, false).highlight(source),
                source
            );
        }
    }

    #[test]
    fn unicode_matching_preserves_original_source_and_byte_boundaries() {
        for (query, source, expected) in [
            (
                "needle",
                "İ 中文 🦀 NEEDLE İ needle",
                "İ 中文 🦀 \x1b[1;33mNEEDLE\x1b[0m İ \x1b[1;33mneedle\x1b[0m",
            ),
            ("中文", "🦀 中文", "🦀 \x1b[1;33m中文\x1b[0m"),
            ("Äpfel", "äPFEL", "\x1b[1;33mäPFEL\x1b[0m"),
        ] {
            assert_eq!(
                QueryHighlighter::new(query, true).highlight(source),
                expected
            );
        }
    }
}
