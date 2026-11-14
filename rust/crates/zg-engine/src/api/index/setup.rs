//! Model choices shared by terminal and MCP index setup.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexSetupOptions {
    /// In-process workspace analysis only; daemon v2 setup remains catalog-only.
    #[serde(skip)]
    pub root: Option<PathBuf>,
    #[serde(skip)]
    pub scan: super::options::ScanRulesUpdate,
    pub model_cache: Option<PathBuf>,
    /// A caller's model preference, without authorizing index creation.
    pub preferred_model: Option<String>,
    /// Whether this request supplies an API credential. Never contains the credential.
    pub credential_available: bool,
    #[serde(skip)]
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IndexSetup {
    pub models: Vec<EmbeddingChoice>,
    pub default_model: Option<String>,
    /// Supports caller defaults without treating them as explicit overrides.
    #[serde(default)]
    pub supports_embedding_defaults: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<super::embedding::EmbeddingSelection>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "These independent catalog and availability flags are not lifecycle states"
)]
pub struct EmbeddingChoice {
    pub reference: String,
    pub local: bool,
    pub downloaded: bool,
    pub configured: bool,
    pub credential_available: bool,
    pub download_bytes: Option<u64>,
}

impl EmbeddingChoice {
    #[must_use]
    pub const fn available(&self) -> bool {
        self.local || self.credential_available
    }
}
