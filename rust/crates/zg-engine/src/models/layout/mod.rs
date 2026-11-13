//! Layout inference contracts, independent of PDF parsing and text assembly.

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::ModelError;
use crate::domain::model::ModelProgress;

#[derive(Clone, Debug, Default)]
pub(crate) struct LayoutModelConfig {
    pub cache_dir: Option<PathBuf>,
}

#[derive(Clone)]
pub(crate) struct LayoutImage {
    pub width: usize,
    pub height: usize,
    pub rgb: Arc<[u8]>,
}

impl LayoutImage {
    pub(crate) fn validate(&self) -> Result<(), ModelError> {
        if self.width == 0
            || self.height == 0
            || self
                .width
                .checked_mul(self.height)
                .and_then(|size| size.checked_mul(3))
                != Some(self.rgb.len())
            || self.rgb.len() > 64 * 1024 * 1024
        {
            return Err(ModelError::new(
                crate::EngineError::INVALID_ARGUMENT,
                "Invalid RGB layout image",
                None,
            ));
        }
        Ok(())
    }
}

/// Model-neutral region categories. Image coordinates use a top-left origin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LayoutKind {
    Heading,
    Image,
    Text,
    PageNumber,
    Abstract,
    Contents,
    FigureCaption,
    Formula,
    Table,
    TableCaption,
    Reference,
    DocumentTitle,
    Footnote,
    Header,
    Algorithm,
    Footer,
    Seal,
    ChartCaption,
    Chart,
    FormulaNumber,
    HeaderImage,
    FooterImage,
    Aside,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LayoutRegion {
    pub kind: LayoutKind,
    pub confidence: f32,
    pub bounds: [f32; 4],
}

#[derive(Clone, Default)]
pub(crate) struct LayoutOptions {
    pub signal: Option<CancellationToken>,
    pub on_progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
    /// Keeps runtime ownership and admission until native work completes.
    pub execution_lease: Option<Arc<dyn Send + Sync>>,
}

#[async_trait]
pub(crate) trait LayoutModel: Send + Sync {
    async fn prepare(&self, options: LayoutOptions) -> Result<(), ModelError>;
    async fn detect(
        &self,
        image: LayoutImage,
        options: LayoutOptions,
    ) -> Result<Vec<LayoutRegion>, ModelError>;
}
