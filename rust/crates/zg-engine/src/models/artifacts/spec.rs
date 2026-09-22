//! Immutable artifact specifications shared by the catalog and downloader.

use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ArtifactSourceKind {
    HuggingFace,
    ModelScope,
}

impl ArtifactSourceKind {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::HuggingFace => "huggingface",
            Self::ModelScope => "modelscope",
        }
    }

    pub(super) const fn display_name(self) -> &'static str {
        match self {
            Self::HuggingFace => "Hugging Face",
            Self::ModelScope => "ModelScope",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactSourceConfig {
    pub(crate) repo: &'static str,
    pub(crate) revision: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactConfig {
    pub(crate) path: &'static str,
    pub(crate) size: u64,
    pub(crate) sha256: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactDownloadConfig {
    pub(crate) hugging_face: ArtifactSourceConfig,
    pub(crate) model_scope: ArtifactSourceConfig,
    pub(crate) artifacts: &'static [ArtifactConfig],
}
