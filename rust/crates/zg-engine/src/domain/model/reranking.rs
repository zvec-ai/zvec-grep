//! Shared result contract for a future reranking backend.

/// Original candidate position and its relevance score. Results may be reordered or partial.
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)] // The data contract is defined before the first reranking backend.
pub(crate) struct RerankScore {
    pub index: usize,
    pub score: f32,
}
