//! Node-API binding shared by the legacy TypeScript engine.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Node-API decodes owned JavaScript arguments; map_err consumes native errors"
)]

use napi_derive::napi;

use crate::{Distance, Record, VectorIndex};

#[napi(object)]
pub struct Embedding {
    pub document_id: String,
    pub vector: Vec<f64>,
}

#[napi(object)]
pub struct SearchHit {
    pub document_id: String,
    pub score: f64,
}

#[napi(js_name = "DiskannIndex")]
pub struct NodeIndex {
    index: Option<VectorIndex>,
}

#[napi]
impl NodeIndex {
    #[napi(constructor)]
    pub fn new(
        path: String,
        dimension: u32,
        metric: String,
        read_only: bool,
    ) -> napi::Result<Self> {
        if read_only {
            return VectorIndex::open_read_only(std::path::Path::new(&path))
                .map(|index| Self { index: Some(index) })
                .map_err(js_error);
        }
        let distance = match metric.as_str() {
            "cosine" => Distance::Cosine,
            "dot" => Distance::DotProduct,
            "euclidean" => Distance::Euclidean,
            _ => {
                return Err(napi::Error::from_reason(format!(
                    "unsupported vector metric: {metric}"
                )));
            }
        };
        let index = VectorIndex::open(
            std::path::Path::new(&path),
            dimension as usize,
            distance,
            read_only,
        )
        .map_err(js_error)?;
        Ok(Self { index: Some(index) })
    }

    #[napi]
    pub fn upsert(&self, entries: Vec<Embedding>) -> napi::Result<()> {
        let vectors = entries
            .iter()
            .map(|entry| vector(&entry.vector))
            .collect::<napi::Result<Vec<_>>>()?;
        let records = entries
            .iter()
            .zip(&vectors)
            .map(|(entry, vector)| Record {
                document_id: &entry.document_id,
                file_id: 0,
                vector,
            })
            .collect::<Vec<_>>();
        self.get()?.upsert(&records).map_err(js_error)
    }

    #[napi]
    pub fn delete_documents(&self, ids: Vec<String>) -> napi::Result<()> {
        self.get()?.delete_documents(&ids).map_err(js_error)
    }

    #[napi]
    pub fn search(
        &self,
        query: Vec<f64>,
        limit: u32,
        allowed: Option<Vec<String>>,
    ) -> napi::Result<Vec<SearchHit>> {
        let allowed = allowed.map(|ids| ids.into_iter().collect());
        self.get()?
            .search(&vector(&query)?, limit as usize, allowed.as_ref())
            .map_err(js_error)
            .map(|hits| {
                hits.into_iter()
                    .map(|hit| SearchHit {
                        document_id: hit.document_id,
                        score: hit.score,
                    })
                    .collect()
            })
    }

    #[napi]
    pub fn close(&mut self) {
        self.index.take();
    }
}

impl NodeIndex {
    fn get(&self) -> napi::Result<&VectorIndex> {
        self.index
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("DiskANN index is closed"))
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "embeddings are validated then narrowed to the shared FP32 format"
)]
fn vector(values: &[f64]) -> napi::Result<Vec<f32>> {
    if values
        .iter()
        .any(|value| !value.is_finite() || value.abs() > f64::from(f32::MAX))
    {
        return Err(napi::Error::from_reason(
            "embedding values must be finite FP32 numbers",
        ));
    }
    Ok(values.iter().map(|value| *value as f32).collect())
}

fn js_error(error: crate::Error) -> napi::Error {
    napi::Error::from_reason(error.to_string())
}
