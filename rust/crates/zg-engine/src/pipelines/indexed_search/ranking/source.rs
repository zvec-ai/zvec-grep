//! Source evidence for direct lookups in the leading recalled fragment.
use super::{Content, Range, SearchHit};

pub(super) const MAX_BYTES: usize = 8 * 1024;

pub(super) fn best_fragment(hit: &SearchHit) -> Option<&str> {
    let Content::Code(source) = hit.entity.content() else {
        return None;
    };
    match hit.evidence.first()?.fragment.range {
        Range::Full => Some(source),
        Range::Byte(range) => source.get(
            usize::try_from(range.start_offset()).ok()?
                ..usize::try_from(range.end_offset()).ok()?,
        ),
        Range::Text(_) => None,
    }
}

pub(super) fn literal_phrase(query: &str) -> Option<&str> {
    let mut phrase = query.trim();
    if let Some(quote @ ('\'' | '"' | '`')) = phrase.chars().next()
        && let Some(inner) = phrase[1..].strip_suffix(quote)
    {
        phrase = inner;
    }
    (phrase.len() <= MAX_BYTES
        && !phrase.contains(['\n', '\r'])
        && phrase.split_whitespace().nth(1).is_some())
    .then_some(phrase)
}

/// A complete string argument in test code is direct evidence for a pasted
/// test title. A substring of a longer title keeps the regular path prior.
pub(super) fn has_literal_argument(hit: &SearchHit, phrase: &str) -> bool {
    let Some(fragment) = best_fragment(hit) else {
        return false;
    };
    let prefix = &fragment[..fragment.floor_char_boundary(MAX_BYTES)];
    prefix.match_indices(phrase).any(|(start, _)| {
        let before = &fragment[..start];
        let Some(quote @ ('\'' | '"' | '`')) = before.chars().next_back() else {
            return false;
        };
        let after = &prefix[start + phrase.len()..];
        before[..before.len() - 1].trim_end().ends_with(['(', ','])
            && after
                .strip_prefix(quote)
                .is_some_and(|rest| rest.trim_start().starts_with([',', ')']))
    })
}
