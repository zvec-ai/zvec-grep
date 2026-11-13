//! Bounded lexical support from individual recalled source windows.
use std::collections::HashSet;

use super::{SearchHit, query};
use crate::domain::{Content, Range};

const MAX_QUERY_BYTES: usize = 8_192;
const MAX_QUERY_TERMS: usize = 64;
const MAX_TEXT_BYTES: usize = 32_768;
const MAX_FRAGMENTS: usize = 32;
const WINDOW_TOKENS: usize = 128;

pub(super) struct BodyQuery {
    terms: HashSet<String>,
}

impl BodyQuery {
    pub(super) fn new(query: &str) -> Option<Self> {
        if query.len() > MAX_QUERY_BYTES {
            return None;
        }
        let terms = words(query)
            .into_iter()
            .filter(|word| informative(word))
            .collect::<HashSet<_>>();
        (2..=MAX_QUERY_TERMS)
            .contains(&terms.len())
            .then_some(Self { terms })
    }

    pub(super) fn support(&self, hit: &SearchHit) -> f64 {
        let Content::Text(text) = &hit.entity.content else {
            return 0.0;
        };
        // Recalled fragment ranges are entity-relative. Do not join separate
        // fragments or use a grouped entity outline as extra source evidence.
        if hit.evidence.is_empty() {
            return self.window_support(text);
        }
        let mut seen = HashSet::new();
        hit.evidence
            .iter()
            .filter(|evidence| seen.insert(&evidence.fragment.id))
            .take(MAX_FRAGMENTS)
            .filter_map(|evidence| match evidence.fragment.range {
                Range::Full => Some(text.as_str()),
                Range::Byte(range) => {
                    let start = usize::try_from(range.start_offset()).ok()?;
                    let end = usize::try_from(range.end_offset()).ok()?;
                    text.get(start..end)
                }
                Range::Text(_) => None,
            })
            .map(|window| self.window_support(window))
            .fold(0.0, f64::max)
    }

    fn window_support(&self, text: &str) -> f64 {
        let text = &text[..text.floor_char_boundary(MAX_TEXT_BYTES.min(text.len()))];
        let tokens = words(text);
        // Bound proximity even inside a large class or a full-content fragment.
        // Overlap avoids treating a fixed window boundary as a source boundary.
        (0..tokens.len())
            .step_by(WINDOW_TOKENS / 2)
            .map(|start| {
                let matched = tokens[start..(start + WINDOW_TOKENS).min(tokens.len())]
                    .iter()
                    .filter(|word| self.terms.contains(*word))
                    .collect::<HashSet<_>>()
                    .len();
                if matched < 2 {
                    return 0.0;
                }
                f64::from(u32::try_from(matched).unwrap_or(0))
                    / f64::from(u32::try_from(self.terms.len()).unwrap_or(1))
            })
            .fold(0.0, f64::max)
    }
}

fn words(text: &str) -> Vec<String> {
    query::tokens(text)
}

fn informative(word: &str) -> bool {
    word.chars().count() >= 2
        && !matches!(
            word,
            "a" | "an"
                | "the"
                | "and"
                | "or"
                | "of"
                | "to"
                | "in"
                | "on"
                | "with"
                | "for"
                | "from"
                | "by"
                | "at"
                | "as"
                | "is"
                | "are"
                | "was"
                | "were"
                | "be"
                | "been"
                | "being"
                | "how"
                | "what"
                | "where"
                | "when"
                | "why"
                | "does"
                | "do"
                | "did"
                | "can"
                | "could"
                | "should"
                | "would"
                | "will"
                | "that"
                | "this"
                | "these"
                | "those"
                | "it"
                | "its"
                | "they"
                | "their"
                | "them"
                | "we"
                | "you"
                | "your"
                | "after"
                | "before"
                | "than"
                | "then"
                | "into"
                | "across"
        )
}

#[cfg(test)]
mod tests {
    // Exact zero/full coverage and repetition invariance are part of the contract.
    #![allow(clippy::float_cmp)]
    use super::*;
    use crate::domain::{ByteRange, EntityFragment, FragmentId};
    use crate::pipelines::indexed_search::{pipeline::SearchEvidence, ranking::tests::hit};

    fn candidate(text: &str) -> SearchHit {
        let mut candidate = hit("body", 1, "src/manager.rs", 1.0, 1, None);
        candidate.entity.content = Content::Text(text.into());
        candidate
    }

    #[test]
    fn distinct_subwords_support_body_without_rewarding_repetition() {
        let query = BodyQuery::new("how does connection pool timeout work").expect("query");
        let short = query.support(&candidate("connectionPool timeout"));
        assert!(short > 0.5);
        assert_eq!(
            short,
            query.support(&candidate(&"connectionPool timeout ".repeat(20)))
        );
        assert_eq!(query.support(&candidate("connection connection")), 0.0);
        assert_eq!(
            BodyQuery::new("HTTP request")
                .expect("query")
                .support(&candidate("HTTPRequest")),
            1.0
        );
    }

    #[test]
    fn separate_fragments_cannot_manufacture_cooccurrence() {
        let query = BodyQuery::new("network backoff").expect("query");
        let mut split = candidate("network backoff");
        split.evidence = [(0, 7), (8, 15)]
            .into_iter()
            .enumerate()
            .map(|(i, (start, end))| SearchEvidence {
                fragment: EntityFragment {
                    id: FragmentId::new(&split.entity.id, u32::try_from(i).expect("ordinal")),
                    range: Range::Byte(ByteRange::new(start, end).expect("range")),
                },
            })
            .collect();
        assert_eq!(query.support(&split), 0.0);
        assert_eq!(query.support(&candidate("network backoff")), 1.0);
        split.evidence.push(split.evidence[0].clone());
        assert_eq!(query.support(&split), 0.0);
    }

    #[test]
    fn large_entities_and_unicode_ranges_remain_bounded() {
        let query = BodyQuery::new("network backoff").expect("query");
        assert_eq!(
            query.support(&candidate(&format!(
                "network {} backoff",
                "padding ".repeat(256)
            ))),
            0.0
        );
        assert_eq!(
            query.support(&candidate(&format!(
                "{}network backoff",
                "界".repeat(11_000)
            ))),
            0.0
        );
        let mut invalid = candidate("界 network backoff");
        invalid.evidence.push(SearchEvidence {
            fragment: EntityFragment {
                id: FragmentId::new(&invalid.entity.id, 0),
                range: Range::Byte(ByteRange::new(1, 20).expect("range")),
            },
        });
        assert_eq!(query.support(&invalid), 0.0);
        let chinese = BodyQuery::new("网络重试 退避策略").expect("query");
        assert_eq!(chinese.support(&candidate("网络重试 退避策略")), 1.0);
        assert_eq!(chinese.support(&candidate("网络 退避")), 0.0);
        assert!(BodyQuery::new(&"q".repeat(MAX_QUERY_BYTES + 1)).is_none());
        assert!(BodyQuery::new("the how and").is_none());
        assert!(BodyQuery::new("network network").is_none());
        assert!(
            BodyQuery::new(
                &(0..65)
                    .map(|i| format!("term{i}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
            .is_none()
        );
    }
}
