use crate::extraction::ChunkOptions;

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
    let max_chunk_chars = max_input_tokens.saturating_mul(chars_per_100_tokens) / 100;
    ChunkOptions {
        max_chunk_chars: Some(max_chunk_chars.max(1)),
        chunk_overlap_chars: Some(max_chunk_chars.saturating_mul(CHUNK_OVERLAP_PERCENT) / 100),
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
    }
}
