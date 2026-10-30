//! Bounded sequence packing for the non-causal Gemma encoder.
use std::ops::Range;

use super::{
    IndexedEmbedding, ModelError, PreparedLlamaInput, check_cancelled, llama_batch_capacity,
};
use tokio_util::sync::CancellationToken;

// Non-causal attention builds a mask over the whole batch. Bound total tokens,
// not just sequence count; a longer valid input is executed on its own.
const GEMMA_BATCH_TOKENS: usize = 128;

pub(super) struct LlamaBatchPlan {
    ranges: Vec<Range<usize>>,
    pub(super) token_capacity: u32,
    pub(super) sequence_capacity: u32,
}

impl LlamaBatchPlan {
    pub(super) fn new(
        inputs: &[PreparedLlamaInput],
        context_size: u32,
        multisequence: bool,
    ) -> Result<Self, ModelError> {
        let token_budget = GEMMA_BATCH_TOKENS.min(context_size as usize);
        let mut ranges = Vec::new();
        let mut start = 0;
        let mut tokens = 0;
        let mut max_tokens = 1;
        let mut max_sequences = 1;
        for (index, input) in inputs.iter().enumerate() {
            let length = input.tokens.len();
            if length == 0 || length > context_size as usize {
                return Err(ModelError::invalid_argument(
                    "llama.cpp embedding input must fit the nonempty context",
                ));
            }
            if index > start && (!multisequence || tokens + length > token_budget) {
                ranges.push(start..index);
                start = index;
                tokens = 0;
            }
            tokens += length;
            max_tokens = max_tokens.max(tokens);
            max_sequences = max_sequences.max(index - start + 1);
        }
        if start < inputs.len() {
            ranges.push(start..inputs.len());
        }
        Ok(Self {
            ranges,
            token_capacity: llama_batch_capacity(max_tokens, context_size)?,
            // Native graph reservation rounds token count to a multiple of
            // n_seq_max, but keeps the original output count for pooling.
            // Power-of-two capacities keep those dimensions aligned.
            sequence_capacity: u32::try_from(max_sequences.next_power_of_two()).map_err(
                |error| {
                    ModelError::internal("Too many llama.cpp embedding sequences").with_cause(error)
                },
            )?,
        })
    }

    pub(super) fn execute(
        &self,
        inputs: &[PreparedLlamaInput],
        signal: Option<&CancellationToken>,
        mut infer: impl FnMut(&[PreparedLlamaInput]) -> Result<Vec<Vec<f32>>, ModelError>,
    ) -> Result<Vec<IndexedEmbedding>, ModelError> {
        let mut output = Vec::with_capacity(inputs.len());
        for range in &self.ranges {
            check_cancelled(signal)?;
            let batch = &inputs[range.clone()];
            let vectors = infer(batch)?;
            check_cancelled(signal)?;
            if vectors.len() != batch.len() {
                return Err(ModelError::internal(
                    "llama.cpp returned the wrong sequence count",
                ));
            }
            output.extend(
                batch
                    .iter()
                    .zip(vectors)
                    .map(|(input, vector)| (input.index, vector)),
            );
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests;
