//! Asynchronous file preparation. Format dependencies stay behind this boundary;
//! callers choose chunk budgets only after the searchable text is available.
use super::{
    ChunkOptions, ExtractionOutput, SourceKind, TextSource,
    pdf::{PdfExtractionPort, PdfExtractionService, PdfMarkdownSource},
};
use crate::{
    EngineError, ErrorReport,
    domain::{FileFormat, SourcePath, model::ModelProgress},
    models::{LayoutModelConfig, ModelRuntimeManager},
    utils::{DecodeFailure, decode_index_text},
};
use async_trait::async_trait;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

pub(crate) struct ExtractionRequest<'a> {
    pub path: &'a SourcePath,
    pub formats: &'a [FileFormat],
    pub bytes: Arc<[u8]>,
    pub signal: CancellationToken,
    pub on_progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
}

pub(crate) enum PreparedSource {
    Text(TextSource),
    Pdf(PdfMarkdownSource),
}
impl PreparedSource {
    pub(crate) fn text(&self) -> &str {
        match self {
            Self::Text(s) => &s.text,
            Self::Pdf(s) => &s.text.text,
        }
    }
    pub(crate) fn extract(
        &self,
        options: ChunkOptions,
        graph: bool,
    ) -> Result<ExtractionOutput, EngineError> {
        let source: super::Source<'_> = match self {
            Self::Text(s) => s.into(),
            Self::Pdf(s) => s.into(),
        };
        if graph {
            super::extract_for_indexing(source, options)
        } else {
            super::extract_without_graph(source, options)
        }
    }
    fn warnings(&self) -> Vec<String> {
        let Self::Pdf(source) = self else {
            return Vec::new();
        };
        let mut warnings = source.warnings.clone();
        if source.text.text.trim().is_empty() {
            warnings.push(format!(
                "{}: no searchable native text remained after PDF extraction; OCR is not enabled",
                source.text.relative_path.display()
            ));
        }
        warnings
    }
}

/// A document failure affects one file; a dependency failure affects its source
/// kind for this operation. Cancellation is always returned immediately.
#[derive(Debug)]
pub(crate) enum ExtractionError {
    Document(EngineError),
    Dependency(EngineError),
    /// The bytes carry no indexable text, so the caller skips the path instead
    /// of failing it; the reason says which cause fired.
    NoText {
        path: SourcePath,
        failure: DecodeFailure,
    },
}
impl From<EngineError> for ExtractionError {
    fn from(error: EngineError) -> Self {
        Self::Document(error)
    }
}
impl From<ExtractionError> for EngineError {
    fn from(error: ExtractionError) -> Self {
        match error {
            ExtractionError::Document(e) | ExtractionError::Dependency(e) => e,
            ExtractionError::NoText { path, .. } => Self::invalid_argument(format!(
                "cannot extract text from {}: binary content or malformed BOM-marked text",
                path.display()
            )),
        }
    }
}

#[async_trait]
pub(crate) trait ExtractionPort: Send + Sync {
    async fn prepare(
        &self,
        request: ExtractionRequest<'_>,
    ) -> Result<PreparedSource, ExtractionError>;
}

pub(crate) struct ExtractionService {
    pdf: Box<dyn PdfExtractionPort>,
}
impl ExtractionService {
    pub(crate) fn new(models: ModelRuntimeManager, cache_dir: Option<PathBuf>) -> Self {
        Self {
            pdf: Box::new(PdfExtractionService::new(
                models,
                LayoutModelConfig { cache_dir },
            )),
        }
    }
    #[cfg(test)]
    pub(crate) fn with_pdf(pdf: impl PdfExtractionPort + 'static) -> Self {
        Self { pdf: Box::new(pdf) }
    }
}
#[cfg(test)]
impl Default for ExtractionService {
    fn default() -> Self {
        Self::new(ModelRuntimeManager::new(), None)
    }
}
#[async_trait]
impl ExtractionPort for ExtractionService {
    async fn prepare(
        &self,
        request: ExtractionRequest<'_>,
    ) -> Result<PreparedSource, ExtractionError> {
        let ExtractionRequest {
            path,
            formats,
            bytes,
            signal,
            on_progress,
        } = request;
        match super::source_kind(formats) {
            Some(SourceKind::Text) => {
                let text = decode_index_text(formats, &bytes).map_err(|failure| {
                    ExtractionError::NoText {
                        path: path.clone(),
                        failure,
                    }
                })?;
                Ok(PreparedSource::Text(TextSource {
                    relative_path: path.clone(),
                    formats: formats.to_vec(),
                    text: text.into_owned(),
                }))
            }
            Some(SourceKind::Pdf) => self
                .pdf
                .convert(path.clone(), bytes, signal, on_progress)
                .await
                .map(PreparedSource::Pdf)
                .map_err(|error| {
                    let dependency = error.disables_pdf();
                    let error = error.into_engine_error();
                    if dependency {
                        ExtractionError::Dependency(error)
                    } else {
                        ExtractionError::Document(error)
                    }
                }),
            _ => Err(EngineError::unsupported(format!(
                "cannot index {}: unsupported source",
                path.display()
            ))
            .into()),
        }
    }
}

/// Dependency failure suppression and warnings live for one indexing operation,
/// including retry passes. A later operation retries with a clean slate.
pub(crate) struct ExtractionOperation<'a> {
    inner: &'a dyn ExtractionPort,
    failed: Mutex<HashMap<SourceKind, ErrorReport>>,
    warnings: Mutex<Vec<String>>,
}
impl<'a> ExtractionOperation<'a> {
    pub(crate) fn new(inner: &'a dyn ExtractionPort) -> Self {
        Self {
            inner,
            failed: Mutex::default(),
            warnings: Mutex::default(),
        }
    }
    pub(crate) fn warnings(&self) -> Vec<String> {
        self.warnings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
#[async_trait]
impl ExtractionPort for ExtractionOperation<'_> {
    async fn prepare(
        &self,
        request: ExtractionRequest<'_>,
    ) -> Result<PreparedSource, ExtractionError> {
        if request.signal.is_cancelled() {
            return Err(EngineError::cancelled("Source extraction cancelled").into());
        }
        let kind = super::source_kind(request.formats);
        if let Some(error) = kind.and_then(|kind| {
            self.failed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&kind)
                .cloned()
        }) {
            return Err(ExtractionError::Dependency(EngineError::from_report(error)));
        }
        match self.inner.prepare(request).await {
            Err(ExtractionError::Dependency(error)) => {
                if error.code() != EngineError::CANCELLED
                    && let Some(kind) = kind
                {
                    self.failed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(kind, error.report());
                }
                Err(ExtractionError::Dependency(error))
            }
            Ok(source) => {
                for warning in source.warnings() {
                    let mut warnings = self
                        .warnings
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !warnings.contains(&warning) {
                        warnings.push(warning);
                    }
                }
                Ok(source)
            }
            error => error,
        }
    }
}

#[cfg(test)]
mod tests;
