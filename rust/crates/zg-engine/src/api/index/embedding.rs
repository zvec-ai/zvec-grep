//! Explainable embedding choices shared by index creation and interactive setup.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingScene {
    Code,
    Text,
    Mixed,
    Unknown,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EmbeddingEvidence {
    pub files: usize,
    /// Sum of file sizes, with each file contributing at most 64 KiB.
    pub weight_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EmbeddingAnalysis {
    pub scene: EmbeddingScene,
    pub code: EmbeddingEvidence,
    pub text: EmbeddingEvidence,
    /// Data, configuration, ambiguous formats, and files that could not be classified.
    pub other_files: usize,
    pub recommended_model: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingSelectionSource {
    Explicit,
    ExistingIndex,
    Environment,
    Configuration,
    Automatic,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EmbeddingSelection {
    pub model: String,
    pub source: EmbeddingSelectionSource,
    /// Existing indexes and explicit model requests do not need a workspace analysis.
    pub analysis: Option<EmbeddingAnalysis>,
}
