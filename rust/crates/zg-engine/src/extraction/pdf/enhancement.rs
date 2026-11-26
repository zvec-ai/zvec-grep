//! Optional layout filtering. Native extraction and cancellation remain mandatory.
use std::{
    future::Future,
    sync::atomic::{AtomicBool, Ordering},
};

use super::{filter, render::RenderedPage, service::check};
use crate::{
    EngineError,
    domain::SourcePath,
    models::{LayoutImage, LayoutOptions, LayoutRegion, ModelError},
};

pub(super) struct Enhancement<'a> {
    pub disabled: &'a AtomicBool,
    pub path: &'a SourcePath,
    pub options: LayoutOptions,
    pub warnings: Vec<String>,
}

impl Enhancement<'_> {
    pub async fn apply<F, Fut>(
        &mut self,
        page: &mut RenderedPage,
        infer: F,
    ) -> Result<(), EngineError>
    where
        F: FnOnce(LayoutImage) -> Fut,
        Fut: Future<Output = Result<Vec<LayoutRegion>, ModelError>>,
    {
        self.check()?;
        if self.disabled.load(Ordering::Relaxed) {
            self.warn(
                page.page.page_number,
                "layout model unavailable for this indexing operation",
            );
            return Ok(());
        }
        let image = match std::mem::replace(&mut page.image, Ok(None)) {
            Ok(Some(image)) => image,
            Ok(None) => return Ok(()),
            Err(error) => {
                self.warn(page.page.page_number, &error);
                return Ok(());
            }
        };
        let width = f64::from(
            u32::try_from(image.width)
                .map_err(|_| EngineError::internal("PDF raster width overflow"))?,
        );
        let height = f64::from(
            u32::try_from(image.height)
                .map_err(|_| EngineError::internal("PDF raster height overflow"))?,
        );
        let predicted = infer(image).await;
        self.check()?;
        let regions = match predicted {
            Ok(regions) => regions,
            Err(error) => {
                if error.code() == EngineError::CANCELLED {
                    return Err(error.into_engine_error());
                }
                if error.should_fail_fast() {
                    self.disabled.store(true, Ordering::Relaxed);
                }
                self.warn(page.page.page_number, &error.to_string());
                return Ok(());
            }
        };
        // Validate the whole prediction before removing any native text.
        if regions.iter().any(|region| {
            !region.confidence.is_finite() || !region.bounds.iter().all(|value| value.is_finite())
        }) {
            self.warn(
                page.page.page_number,
                "layout prediction contains invalid coordinates",
            );
            return Ok(());
        }
        let regions = regions
            .into_iter()
            .map(|region| filter::Region {
                kind: region.kind,
                bounds: std::array::from_fn(|i| {
                    f64::from(region.bounds[i])
                        * if i % 2 == 0 {
                            f64::from(page.page.page_width) / width
                        } else {
                            f64::from(page.page.page_height) / height
                        }
                }),
            })
            .collect();
        filter::apply(&mut page.page, regions, &page.tables);
        Ok(())
    }

    fn check(&self) -> Result<(), EngineError> {
        if let Some(signal) = &self.options.signal {
            check(signal)?;
        }
        Ok(())
    }

    fn warn(&mut self, page: usize, reason: &str) {
        let message = format!(
            "{}: page {page}: using native PDF text without layout filtering: {reason}",
            self.path.display()
        );
        if let Some(report) = &self.options.on_progress {
            report(crate::domain::model::ModelProgress::Skipped {
                model_ref: "PDF layout".into(),
                message: message.clone(),
            });
        }
        self.warnings.push(message);
    }
}

#[cfg(test)]
mod tests;
