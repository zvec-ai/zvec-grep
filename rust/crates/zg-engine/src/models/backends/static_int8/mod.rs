//! Static row-wise INT8 embeddings without an ONNX execution session.

mod batch;
mod model;
mod onnx;
mod protobuf;
mod table;

pub(crate) use model::StaticInt8EmbeddingModel;
