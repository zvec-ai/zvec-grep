use super::{PdfMarkdownSource, assembly, filter, render};
use crate::{
    EngineError,
    domain::{SourcePath, model::ModelProgress},
    models::{LayoutModelConfig, LayoutOptions, ModelError, ModelRuntimeManager},
};
use async_trait::async_trait;
use std::{sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub(crate) enum PdfExtractionError {
    Document(EngineError),
    Model(ModelError),
    Runtime(EngineError),
}
impl From<EngineError> for PdfExtractionError {
    fn from(e: EngineError) -> Self {
        Self::Document(e)
    }
}
impl From<ModelError> for PdfExtractionError {
    fn from(e: ModelError) -> Self {
        Self::Model(e)
    }
}
impl PdfExtractionError {
    pub(crate) fn should_abort(&self) -> bool {
        match self {
            Self::Model(e) => e.code() == EngineError::CANCELLED,
            Self::Document(e) | Self::Runtime(e) => e.code() == EngineError::CANCELLED,
        }
    }
    pub(crate) fn disables_pdf(&self) -> bool {
        match self {
            Self::Model(e) => e.should_fail_fast() && !self.should_abort(),
            Self::Runtime(_) => !self.should_abort(),
            Self::Document(_) => false,
        }
    }
    pub(crate) fn into_engine_error(self) -> EngineError {
        match self {
            Self::Document(e) | Self::Runtime(e) => e,
            Self::Model(e) => e.into_engine_error(),
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
}
impl PdfExtractionService {
    pub(crate) fn new(models: ModelRuntimeManager, config: LayoutModelConfig) -> Self {
        Self { models, config }
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
        let worker =
            tokio::task::spawn_blocking(move || render::produce(&bytes, &sender, &worker_token));
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
                let regions = if let Some(image) = page.image {
                    if layout.is_none() {
                        let model = self.models.acquire_layout(self.config.clone())?;
                        model.prepare(LayoutOptions {
                            signal: Some(signal.clone()),
                            on_progress: on_progress.clone(),
                            ..LayoutOptions::default()
                        }).await?;
                        layout = Some(model);
                    }
                    let width = f64::from(u32::try_from(image.width).map_err(|_| EngineError::internal("PDF raster width overflow"))?);
                    let height = f64::from(u32::try_from(image.height).map_err(|_| EngineError::internal("PDF raster height overflow"))?);
                    layout.as_ref().expect("prepared layout").detect(image, LayoutOptions {
                        signal: Some(signal.clone()),
                        ..LayoutOptions::default()
                    }).await?.into_iter().map(|r| filter::Region {
                        kind: r.kind,
                        bounds: std::array::from_fn(|i| f64::from(r.bounds[i]) * if i % 2 == 0 {
                            f64::from(page.page.page_width) / width
                        } else {
                            f64::from(page.page.page_height) / height
                        }),
                    }).collect()
                } else {
                    Vec::new()
                };
                check(&signal)?;
                filter::apply(&mut page.page, regions, &page.tables);
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
        let page_count = pages.len();
        let assembly_signal = signal.clone();
        let result = tokio::task::spawn_blocking(move || {
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
