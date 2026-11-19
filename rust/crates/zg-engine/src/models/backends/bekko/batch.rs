use rayon::prelude::*;
use tokenizers::Tokenizer;
use tokio_util::sync::CancellationToken;

use crate::{domain::model::EmbeddingResult, models::spi::ModelError};

use super::table::{Table, check_cancelled};

pub(super) fn embed_texts(
    texts: &[String],
    tokenizer: &Tokenizer,
    table: &Table,
    limit: usize,
    signal: Option<&CancellationToken>,
) -> Result<EmbeddingResult, ModelError> {
    collect_batch(texts, signal, |text| {
        let encoding = tokenizer
            .encode(text, false)
            .map_err(|e| ModelError::invalid_argument("Bekko tokenization failed").with_cause(e))?;
        let ids = encoding.get_ids();
        let vector = table.embed(&ids[..ids.len().min(limit)], signal)?;
        Ok((vector, ids.len() > limit))
    })
}

fn collect_batch<F>(
    texts: &[String],
    signal: Option<&CancellationToken>,
    embed: F,
) -> Result<EmbeddingResult, ModelError>
where
    F: Fn(&str) -> Result<(Vec<f32>, bool), ModelError> + Sync,
{
    check_cancelled(signal)?;
    let apply = |text: &String| {
        check_cancelled(signal)?;
        embed(text)
    };
    // The caller runs inside ModelComputeRuntime's bounded Rayon pool. Nested
    // parallel iteration reuses that pool, including across concurrent batches.
    // Each text keeps its sequential FP32 accumulation and normalization order.
    let rows: Vec<_> = if texts.len() <= 1 {
        texts.iter().map(apply).collect()
    } else {
        texts.par_iter().map(apply).collect()
    };
    // Collect the indexed results before propagating errors: preserve both
    // vector/truncation order and the first error in input order, not race order.
    let mut vectors = Vec::with_capacity(rows.len());
    let mut truncated = Vec::new();
    for (index, row) in rows.into_iter().enumerate() {
        let (vector, was_truncated) = row?;
        vectors.push(vector);
        if was_truncated {
            truncated.push(index);
        }
    }
    check_cancelled(signal)?;
    Ok(EmbeddingResult { vectors, truncated })
}

#[cfg(test)]
#[path = "batch/tests.rs"]
mod tests;
