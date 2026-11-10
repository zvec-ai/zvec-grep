use std::{
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
};

use async_trait::async_trait;
use ort::{
    session::{Session, builder::GraphOptimizationLevel},
    value::Tensor,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use super::preprocess::{SIDE, preprocess};
use crate::models::{
    ModelError,
    artifacts::{
        ArtifactHttpClient, ArtifactSource, LocalModelBackend, ModelDownloadProgressReporter,
        ResolveArtifacts, default_model_cache, resolve_model_artifacts,
    },
    catalog::layout::{PP_S_ARTIFACTS, PP_S_REFERENCE, PP_S_SOURCE},
    layout::{
        LayoutImage, LayoutKind, LayoutModel, LayoutModelConfig, LayoutOptions, LayoutRegion,
    },
    runtime::{ModelComputeRuntime, onnx},
};

pub(crate) struct PpDocLayoutModel {
    cache_dir: PathBuf,
    client: ArtifactHttpClient,
    compute: ModelComputeRuntime,
    session: AsyncMutex<Option<Arc<Mutex<Session>>>>,
}

impl PpDocLayoutModel {
    pub(crate) fn new(config: LayoutModelConfig, compute: ModelComputeRuntime) -> Self {
        let cache_dir = config
            .cache_dir
            .or_else(|| std::env::var_os("ZVEC_GREP_MODEL_CACHE").map(PathBuf::from))
            .unwrap_or_else(|| default_model_cache(LocalModelBackend::Layout))
            .join("layout/pp-doclayout-s")
            .join(PP_S_SOURCE.revision);
        Self {
            cache_dir,
            client: ArtifactHttpClient::new(),
            compute,
            session: AsyncMutex::new(None),
        }
    }

    async fn ensure_loaded(
        &self,
        options: &LayoutOptions,
    ) -> Result<Arc<Mutex<Session>>, ModelError> {
        check_cancelled(options.signal.as_ref())?;
        let mut session = if let Some(signal) = &options.signal {
            tokio::select! {
                guard = self.session.lock() => guard,
                () = signal.cancelled() => return Err(ModelError::cancelled("Layout preparation was cancelled")),
            }
        } else {
            self.session.lock().await
        };
        if let Some(session) = &*session {
            return Ok(Arc::clone(session));
        }
        let reporter = ModelDownloadProgressReporter::new(
            PP_S_REFERENCE,
            options.on_progress.clone(),
            PP_S_ARTIFACTS
                .iter()
                .map(|artifact| artifact.path.to_owned()),
        );
        reporter.start();
        let artifacts = resolve_model_artifacts(
            &self.client,
            ResolveArtifacts {
                model: PP_S_REFERENCE,
                sources: [ArtifactSource::hugging_face(
                    PP_S_SOURCE,
                    self.cache_dir.clone(),
                )],
                artifacts: PP_S_ARTIFACTS,
                reporter: &reporter,
                signal: options.signal.as_ref(),
            },
        )
        .await?;
        let path = artifacts.paths[PP_S_ARTIFACTS[0].path].clone();
        let loaded = self
            .compute
            .run(move || {
                let environment = onnx::initialize()?;
                let session = Session::builder(&environment)
                    .map_err(inference_error)?
                    .with_intra_threads(1)
                    .map_err(inference_error)?
                    .with_inter_threads(1)
                    .map_err(inference_error)?
                    .with_parallel_execution(false)
                    .map_err(inference_error)?
                    .with_optimization_level(GraphOptimizationLevel::All)
                    .map_err(inference_error)?
                    .commit_from_file(path)
                    .map_err(inference_error)?;
                Ok::<_, ModelError>(Arc::new(Mutex::new(session)))
            })
            .await??;
        check_cancelled(options.signal.as_ref())?;
        *session = Some(Arc::clone(&loaded));
        reporter.finish();
        Ok(loaded)
    }
}

#[async_trait]
impl LayoutModel for PpDocLayoutModel {
    async fn prepare(&self, options: LayoutOptions) -> Result<(), ModelError> {
        self.ensure_loaded(&options)
            .await
            .map(|_| ())
            .map_err(ModelError::shared)
    }

    async fn detect(
        &self,
        image: LayoutImage,
        options: LayoutOptions,
    ) -> Result<Vec<LayoutRegion>, ModelError> {
        image.validate()?;
        let session = self
            .ensure_loaded(&options)
            .await
            .map_err(ModelError::shared)?;
        self.compute
            .run(move || {
                check_cancelled(options.signal.as_ref())?;
                let values = preprocess(&image)?;
                let tensor =
                    Tensor::from_array(([1_usize, 3, SIDE, SIDE], values.into_boxed_slice()))
                        .map_err(inference_error)?;
                let scale = scale_factor(image.width, image.height);
                let scale = Tensor::from_array(([1_usize, 2], Vec::from(scale).into_boxed_slice()))
                    .map_err(inference_error)?;
                let mut session = session.lock().unwrap_or_else(PoisonError::into_inner);
                check_cancelled(options.signal.as_ref())?;
                let output = session
                    .run(ort::inputs!["image" => tensor, "scale_factor" => scale])
                    .map_err(inference_error)?;
                let rows = output
                    .get("fetch_name_0")
                    .ok_or_else(|| ModelError::internal("PP-S has no detection output"))?;
                let (shape, values) = rows.try_extract_tensor::<f32>().map_err(inference_error)?;
                if shape.len() != 2 || shape[1] != 6 || values.len() % 6 != 0 {
                    return Err(ModelError::internal("Unexpected PP-S output shape"));
                }
                let count = output
                    .get("fetch_name_1")
                    .ok_or_else(|| ModelError::internal("PP-S has no detection count"))?;
                let (_, count) = count.try_extract_tensor::<i32>().map_err(inference_error)?;
                let count = count
                    .first()
                    .copied()
                    .and_then(|count| usize::try_from(count).ok())
                    .filter(|count| *count <= values.len() / 6)
                    .ok_or_else(|| ModelError::internal("Invalid PP-S detection count"))?;
                check_cancelled(options.signal.as_ref())?;
                decode(&values[..count * 6], image.width, image.height)
            })
            .await
            .map_err(ModelError::shared)?
            .map_err(ModelError::shared)
    }
}

#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "The pinned input contract computes scale in f64 before converting to f32"
)]
fn scale_factor(width: usize, height: usize) -> [f32; 2] {
    [
        (SIDE as f64 / height as f64) as f32,
        (SIDE as f64 / width as f64) as f32,
    ]
}

const LABELS: [LayoutKind; 23] = [
    LayoutKind::Heading,
    LayoutKind::Image,
    LayoutKind::Text,
    LayoutKind::PageNumber,
    LayoutKind::Abstract,
    LayoutKind::Contents,
    LayoutKind::FigureCaption,
    LayoutKind::Formula,
    LayoutKind::Table,
    LayoutKind::TableCaption,
    LayoutKind::Reference,
    LayoutKind::DocumentTitle,
    LayoutKind::Footnote,
    LayoutKind::Header,
    LayoutKind::Algorithm,
    LayoutKind::Footer,
    LayoutKind::Seal,
    LayoutKind::ChartCaption,
    LayoutKind::Chart,
    LayoutKind::FormulaNumber,
    LayoutKind::HeaderImage,
    LayoutKind::FooterImage,
    LayoutKind::Aside,
];

#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Image dimensions are bounded and class IDs are checked before casting"
)]
pub(super) fn decode(
    values: &[f32],
    width: usize,
    height: usize,
) -> Result<Vec<LayoutRegion>, ModelError> {
    let mut result = Vec::new();
    let (rows, remainder) = values.as_chunks::<6>();
    if !remainder.is_empty() {
        return Err(ModelError::internal("Incomplete PP-S detection row").shared());
    }
    for &[class, score, x0, y0, x1, y1] in rows {
        if ![class, score, x0, y0, x1, y1]
            .into_iter()
            .all(f32::is_finite)
        {
            return Err(ModelError::internal("PP-S returned non-finite coordinates"));
        }
        if score <= 0.5 || class < 0.0 {
            continue;
        }
        if class.fract() != 0.0 || class >= LABELS.len() as f32 {
            return Err(ModelError::internal(
                "PP-S returned an unknown region class",
            ));
        }
        let bounds = [
            x0.max(0.0),
            y0.max(0.0),
            x1.min(width as f32),
            y1.min(height as f32),
        ];
        if bounds[2] > bounds[0] && bounds[3] > bounds[1] {
            result.push(LayoutRegion {
                kind: LABELS[class as usize],
                confidence: score,
                bounds,
            });
        }
    }
    Ok(result)
}

fn check_cancelled(signal: Option<&CancellationToken>) -> Result<(), ModelError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        Err(ModelError::cancelled("Layout inference was cancelled"))
    } else {
        Ok(())
    }
}

fn inference_error(error: impl std::fmt::Display) -> ModelError {
    ModelError::internal("PP-S ONNX inference failed").with_cause(error)
}
