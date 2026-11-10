//! Pinned layout assets; execution and downloads remain outside the catalog.

use crate::models::artifacts::{ArtifactConfig, ArtifactSourceConfig};

pub(crate) const PP_S_REFERENCE: &str = "layout/pp-doclayout-s";

// This public ONNX export has the same graph as our frozen Paddle2ONNX export.
// All 38 parity inputs produce identical raw and filtered detections on ORT 1.21.
// Upstream weights: https://huggingface.co/PaddlePaddle/PP-DocLayout-S (Apache-2.0).
pub(crate) const PP_S_SOURCE: ArtifactSourceConfig = ArtifactSourceConfig {
    repo: "stefanj0/PP-DocLayout-S-ONNX",
    revision: "7e851b9e5ad66613d7318a3213ab3336ebe0a514",
};

pub(crate) const PP_S_ARTIFACTS: &[ArtifactConfig] = &[ArtifactConfig {
    path: "pp_doclayout_s.onnx",
    size: 4_917_852,
    sha256: "33688dbee1c23e34b81777e97cb428eb40f24b242c02b5f623484959e830aec8",
}];
