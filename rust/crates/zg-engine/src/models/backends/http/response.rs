use serde::Deserialize;

use crate::{
    domain::model::{EmbeddingModelInfo, EmbeddingResult},
    models::spi::{ModelError, validate_result},
};

#[derive(Deserialize)]
struct Response {
    data: Vec<Embedding>,
}

#[derive(Deserialize)]
struct Embedding {
    index: usize,
    embedding: Vec<f32>,
}

pub(super) fn parse_response(
    bytes: &[u8],
    info: &EmbeddingModelInfo,
    input_count: usize,
) -> Result<EmbeddingResult, ModelError> {
    // Do not attach serde errors: invalid values can contain provider secrets or input text.
    let response: Response = serde_json::from_slice(bytes)
        .map_err(|_| invalid_response("HTTP embedding returned an invalid JSON response"))?;
    if response.data.len() != input_count {
        return Err(invalid_response(
            "HTTP embedding returned the wrong number of vectors",
        ));
    }
    let mut vectors = vec![None; input_count];
    for item in response.data {
        let slot = vectors.get_mut(item.index).ok_or_else(|| {
            invalid_response("HTTP embedding returned an out-of-range vector index")
        })?;
        if slot.is_some() {
            return Err(invalid_response(
                "HTTP embedding returned a duplicate vector index",
            ));
        }
        *slot = Some(item.embedding);
    }
    let result = EmbeddingResult {
        vectors: vectors
            .into_iter()
            .map(|vector| {
                vector.ok_or_else(|| {
                    invalid_response("HTTP embedding response is missing a vector index")
                })
            })
            .collect::<Result<_, _>>()?,
        truncated: Vec::new(),
    };
    validate_result(info, input_count, &result).map_err(ModelError::shared)?;
    Ok(result)
}

fn invalid_response(message: &str) -> ModelError {
    ModelError::internal(message).shared()
}
