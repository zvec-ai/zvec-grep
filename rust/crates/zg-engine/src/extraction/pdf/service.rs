use super::{PdfMarkdownSource, assembly, enhancement::Enhancement, render};
use crate::{
    EngineError,
    domain::{SourcePath, model::ModelProgress},
    models::{LayoutModelConfig, LayoutOptions, ModelError, ModelRuntimeManager},
};
use async_trait::async_trait;
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub(crate) enum PdfExtractionError {
    Document(EngineError),
    Runtime(EngineError),
}
impl From<EngineError> for PdfExtractionError {
    fn from(e: EngineError) -> Self {
        Self::Document(e)
    }
}
impl PdfExtractionError {
    pub(crate) fn should_abort(&self) -> bool {
        match self {
            Self::Document(e) | Self::Runtime(e) => e.code() == EngineError::CANCELLED,
        }
    }
    pub(crate) fn disables_pdf(&self) -> bool {
        match self {
            Self::Runtime(_) => !self.should_abort(),
            Self::Document(_) => false,
        }
    }
    pub(crate) fn into_engine_error(self) -> EngineError {
        match self {
            Self::Document(e) | Self::Runtime(e) => e,
        }
    }
}

#[async_trait]
pub(crate) trait PdfExtractionPort: Send + Sync {
    async fn convert(
        &self,
        path: SourcePath,
        bytes: Arc<[u8]>,
        signal: CancellationToken,
        on_progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
    ) -> Result<PdfMarkdownSource, PdfExtractionError>;
}

pub(crate) struct PdfExtractionService {
    models: ModelRuntimeManager,
    config: LayoutModelConfig,
    layout_disabled: Arc<AtomicBool>,
}
impl PdfExtractionService {
    pub(crate) fn new(models: ModelRuntimeManager, config: LayoutModelConfig) -> Self {
        Self {
            models,
            config,
            layout_disabled: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl PdfExtractionPort for PdfExtractionService {
    async fn convert(
        &self,
        path: SourcePath,
        bytes: Arc<[u8]>,
        signal: CancellationToken,
        on_progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
    ) -> Result<PdfMarkdownSource, PdfExtractionError> {
        check(&signal)?;
        let start = Instant::now();
        let worker_signal = signal.child_token();
        // The worker owns PDFium pages. At most one queued image and the current
        // inference image exist; no page handle crosses an async suspension.
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let worker_token = worker_signal.clone();
        let disabled = Arc::clone(&self.layout_disabled);
        let worker = tokio::task::spawn_blocking(move || {
            render::produce(&bytes, &sender, &worker_token, &disabled)
        });
        let mut enhancement = Enhancement {
            disabled: &self.layout_disabled,
            path: &path,
            options: LayoutOptions {
                signal: Some(signal.clone()),
                on_progress: on_progress.clone(),
                ..LayoutOptions::default()
            },
            warnings: Vec::new(),
        };
        let mut pages = Vec::new();
        let mut layout = None;
        let operation = async {
            loop {
                let next = tokio::select! {
                    biased;
                    () = signal.cancelled() => return Err(EngineError::cancelled("PDF extraction cancelled").into()),
                    next = receiver.recv() => next,
                };
                let Some(mut page) = next else { break; };
                enhancement.apply(&mut page, |image| async {
                    if layout.is_none() {
                        let model = self.models.acquire_layout(self.config.clone()).map_err(ModelError::shared)?;
                        model.prepare(LayoutOptions {
                            signal: Some(signal.clone()),
                            on_progress: on_progress.clone(),
                            ..LayoutOptions::default()
                        }).await.map_err(ModelError::shared)?;
                        layout = Some(model);
                    }
                    layout.as_ref().expect("prepared layout").detect(image, LayoutOptions {
                        signal: Some(signal.clone()),
                        ..LayoutOptions::default()
                    }).await
                }).await?;
                pages.push(page.page);
            }
            Ok::<(), PdfExtractionError>(())
        }.await;
        worker_signal.cancel();
        drop(receiver);
        // Join even after an error: cancellation never leaves an unowned native
        // reader mutating state after the indexing operation has returned.
        let produced = worker.await.map_err(|e| {
            PdfExtractionError::Document(EngineError::internal(format!("PDF worker failed: {e}")))
        })?;
        operation?;
        let bookmarks = produced?;
        check(&signal)?;
        let warnings = enhancement.warnings;
        let page_count = pages.len();
        let assembly_signal = signal.clone();
        let mut result = tokio::task::spawn_blocking(move || {
            assembly::finish(pages, bookmarks, path, &assembly_signal)
        })
        .await
        .map_err(|e| EngineError::internal(format!("PDF assembly worker failed: {e}")))??;
        check(&signal)?;
        tracing::debug!(
            pages = page_count,
            elapsed_ms = start.elapsed().as_millis(),
            "PDF native/layout extraction completed"
        );
        result.warnings = warnings;
        Ok(result)
    }
}

pub(super) fn check(signal: &CancellationToken) -> Result<(), EngineError> {
    if signal.is_cancelled() {
        Err(EngineError::cancelled("PDF extraction cancelled"))
    } else {
        Ok(())
    }
}
