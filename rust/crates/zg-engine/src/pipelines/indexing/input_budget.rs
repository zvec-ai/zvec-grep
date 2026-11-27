use crate::{
    api::info::result::{ChunkInputBudget, EmbeddingInputBudget},
    extraction::ChunkOptions,
};

const DEFAULT_CHARS_PER_100_TOKENS: usize = 185;
const TOKEN_DENSE_CHARS_PER_100_TOKENS: usize = 100;
const TOKEN_DENSE_WINDOW_CHARS: usize = 16 * 1024;
const TOKEN_DENSE_WINDOW_STEP_CHARS: usize = 8 * 1024;
const TOKEN_DENSE_PERCENT: usize = 30;
const CHUNK_OVERLAP_PERCENT: usize = 15;

pub(super) fn index_chunk_options(
    max_input_tokens: Option<usize>,
    text: Option<&str>,
) -> ChunkOptions {
    let Some(max_input_tokens) = max_input_tokens else {
        return ChunkOptions::default();
    };
    let chars_per_100_tokens = if is_token_dense_text(text, max_input_tokens) {
        TOKEN_DENSE_CHARS_PER_100_TOKENS
    } else {
        DEFAULT_CHARS_PER_100_TOKENS
    };
    let budget = chunk_budget(max_input_tokens, chars_per_100_tokens);
    ChunkOptions {
        max_chunk_chars: Some(budget.max_length),
        chunk_overlap_chars: Some(budget.overlap),
    }
}

/// Report both policies without reading source text or preparing a model runtime.
pub(super) fn embedding_input_budget(
    max_input_tokens: Option<usize>,
) -> Option<EmbeddingInputBudget> {
    let max_input_tokens = max_input_tokens?;
    Some(EmbeddingInputBudget {
        max_input_tokens,
        unit: "utf16_code_units".to_owned(),
        rounding: "floor".to_owned(),
        overlap_percent: CHUNK_OVERLAP_PERCENT,
        normal: chunk_budget(max_input_tokens, DEFAULT_CHARS_PER_100_TOKENS),
        dense: chunk_budget(max_input_tokens, TOKEN_DENSE_CHARS_PER_100_TOKENS),
    })
}

fn chunk_budget(max_input_tokens: usize, chars_per_100_tokens: usize) -> ChunkInputBudget {
    let max_chunk_chars = max_input_tokens.saturating_mul(chars_per_100_tokens) / 100;
    ChunkInputBudget {
        chars_per_100_tokens,
        max_length: max_chunk_chars.max(1),
        overlap: max_chunk_chars.saturating_mul(CHUNK_OVERLAP_PERCENT) / 100,
    }
}

fn is_token_dense_text(text: Option<&str>, max_input_tokens: usize) -> bool {
    let Some(text) = text else {
        return false;
    };
    let utf16 = text.encode_utf16().collect::<Vec<_>>();
    if utf16.len() <= max_input_tokens.saturating_mul(TOKEN_DENSE_CHARS_PER_100_TOKENS) / 100 {
        return false;
    }
    if utf16.len() <= TOKEN_DENSE_WINDOW_CHARS {
        return is_token_dense_window(&utf16);
    }

    let last_window_start = utf16.len() - TOKEN_DENSE_WINDOW_CHARS;
    let mut start = 0;
    while start <= last_window_start {
        if is_token_dense_window(&utf16[start..start + TOKEN_DENSE_WINDOW_CHARS]) {
            return true;
        }
        start += TOKEN_DENSE_WINDOW_STEP_CHARS;
    }
    !last_window_start.is_multiple_of(TOKEN_DENSE_WINDOW_STEP_CHARS)
        && is_token_dense_window(&utf16[last_window_start..])
}

fn is_token_dense_window(window: &[u16]) -> bool {
    let required = window
        .len()
        .saturating_mul(TOKEN_DENSE_PERCENT)
        .div_ceil(100);
    window
        .iter()
        .filter(|&&unit| {
            let ascii_letter = (u16::from(b'A')..=u16::from(b'Z')).contains(&unit)
                || (u16::from(b'a')..=u16::from(b'z')).contains(&unit);
            !ascii_letter && unit != 0x20 && unit != 0x09
        })
        .take(required)
        .count()
        >= required
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_main_chunk_budget_for_normal_and_token_dense_text() {
        assert_eq!(
            index_chunk_options(Some(100), Some("ordinary words and spaces")),
            ChunkOptions {
                max_chunk_chars: Some(185),
                chunk_overlap_chars: Some(27),
            }
        );
        assert_eq!(
            index_chunk_options(Some(100), Some(&"你".repeat(101))),
            ChunkOptions {
                max_chunk_chars: Some(100),
                chunk_overlap_chars: Some(15),
            }
        );
    }

    #[test]
    fn no_model_token_limit_means_no_chunk_override() {
        assert_eq!(
            index_chunk_options(None, Some("anything")),
            ChunkOptions::default()
        );
        assert_eq!(embedding_input_budget(None), None);
    }

    #[test]
    fn diagnostic_budgets_share_indexing_arithmetic_and_round_down() {
        for (tokens, normal_length, normal_overlap, dense_overlap) in
            [(1_024, 1_894, 284, 153), (128_000, 236_800, 35_520, 19_200)]
        {
            let budget = embedding_input_budget(Some(tokens)).expect("known model limit");
            assert_eq!(budget.max_input_tokens, tokens);
            assert_eq!(budget.unit, "utf16_code_units");
            assert_eq!(budget.rounding, "floor");
            assert_eq!(budget.overlap_percent, 15);
            assert_eq!(
                budget.normal,
                ChunkInputBudget {
                    chars_per_100_tokens: 185,
                    max_length: normal_length,
                    overlap: normal_overlap,
                }
            );
            assert_eq!(
                budget.dense,
                ChunkInputBudget {
                    chars_per_100_tokens: 100,
                    max_length: tokens,
                    overlap: dense_overlap,
                }
            );
            for (text, expected) in [
                ("ordinary words and spaces".to_owned(), budget.normal),
                ("你".repeat(tokens + 1), budget.dense),
            ] {
                assert_eq!(
                    index_chunk_options(Some(tokens), Some(&text)),
                    ChunkOptions {
                        max_chunk_chars: Some(expected.max_length),
                        chunk_overlap_chars: Some(expected.overlap),
                    }
                );
            }
        }
    }
}
