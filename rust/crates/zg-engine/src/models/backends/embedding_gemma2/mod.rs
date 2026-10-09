//! Native ONNX execution of Google's text and image embedding model.

mod model;
mod processor;

pub(super) use model::EmbeddingGemma2Model;

#[cfg(test)]
mod tests;
