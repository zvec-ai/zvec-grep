use std::{env, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use tokenizers::{Tokenizer, TruncationDirection, TruncationParams};
use tokio::{fs, sync::Mutex};
use tokio_util::sync::CancellationToken;

use crate::{
    domain::{
        Content,
        model::{EmbeddingModelInfo, EmbeddingResult, ModelConfig, ModelInfo, ModelProgress},
    },
    models::{
        artifacts::{
            ArtifactSource, LocalModelBackend, ModelDownloadProgressReporter, ResolveArtifacts,
            default_model_cache, resolve_model_artifacts,
        },
        catalog::BekkoConfig,
        runtime::ModelComputeRuntime,
        spi::{
            EmbeddingModel, EmbeddingOptions, EmbeddingPrepareOptions, ModelError, input_text,
            validate_inputs, validate_result,
        },
    },
};

use super::{
    batch, onnx,
    table::{Table, check_cancelled},
};

pub(crate) struct BekkoEmbeddingModel {
    entry: BekkoConfig,
    info: EmbeddingModelInfo,
    cache: PathBuf,
    compute: ModelComputeRuntime,
    client: reqwest::Client,
    state: Mutex<Option<Arc<Loaded>>>,
}

struct Loaded {
    tokenizer: Tokenizer,
    table: Table,
}

impl BekkoEmbeddingModel {
    pub(crate) fn new(
        entry: BekkoConfig,
        options: ModelConfig,
        compute: ModelComputeRuntime,
    ) -> Self {
        Self {
            entry,
            info: EmbeddingModelInfo::new(
                ModelInfo::new(
                    entry.provider.to_owned(),
                    entry.model.to_owned(),
                    [
                        crate::domain::ContentKind::Text,
                        crate::domain::ContentKind::Code,
                    ],
                    entry.max_batch_size,
                )
                .expect("valid Bekko model metadata"),
                entry.dimension,
                entry.metric,
                Some(entry.max_input_tokens),
                None,
            )
            .expect("valid Bekko embedding metadata"),
            cache: options
                .cache_dir
                .or_else(|| env::var_os("ZVEC_GREP_MODEL_CACHE").map(PathBuf::from))
                .unwrap_or_else(|| default_model_cache(LocalModelBackend::Bekko)),
            compute,
            client: reqwest::Client::new(),
            state: Mutex::new(None),
        }
    }

    async fn ensure_loaded(
        &self,
        progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
        signal: Option<&CancellationToken>,
    ) -> Result<Arc<Loaded>, ModelError> {
        check_cancelled(signal)?;
        let cancellation = signal.cloned().unwrap_or_default();
        let mut state = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ModelError::cancelled("Bekko model preparation was cancelled")),
            state = self.state.lock() => state,
        };
        check_cancelled(signal)?;
        if let Some(loaded) = state.as_ref() {
            return Ok(Arc::clone(loaded));
        }
        let loaded = Arc::new(self.load(progress, signal).await?);
        check_cancelled(signal)?;
        *state = Some(Arc::clone(&loaded));
        Ok(loaded)
    }

    async fn load(
        &self,
        progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
        signal: Option<&CancellationToken>,
    ) -> Result<Loaded, ModelError> {
        let reporter = ModelDownloadProgressReporter::new(
            self.entry.reference,
            progress,
            self.entry
                .download
                .artifacts
                .iter()
                .map(|a| a.path.to_owned()),
        );
        reporter.start();
        let source = self.entry.download.hugging_face;
        let sources = std::iter::once(ArtifactSource::hugging_face(
            source,
            self.cache.join(source.repo).join(source.revision),
        ))
        .chain(
            self.entry
                .download
                .fallback_source(&self.cache, "transformers-js"),
        )
        .collect();
        let resolved = resolve_model_artifacts(
            &self.client,
            ResolveArtifacts {
                model: self.entry.reference,
                sources,
                artifacts: self.entry.download.artifacts,
                reporter: &reporter,
                signal,
            },
        )
        .await?;
        check_cancelled(signal)?;
        let model_path = resolved
            .paths
            .get(self.entry.model_file)
            .ok_or_else(|| ModelError::storage_failure("Bekko embedding artifact is missing"))?;
        let tokenizer_path = resolved
            .paths
            .get("tokenizer.json")
            .ok_or_else(|| ModelError::storage_failure("Bekko tokenizer artifact is missing"))?;
        let bytes = fs::read(model_path)
            .await
            .map_err(|e| ModelError::storage_failure("Unable to read Bekko model").with_cause(e))?;
        check_cancelled(signal)?;
        let tokenizer = fs::read(tokenizer_path).await.map_err(|e| {
            ModelError::storage_failure("Unable to read Bekko tokenizer").with_cause(e)
        })?;
        let entry = self.entry;
        let signal = signal.cloned();
        let loaded = self
            .compute
            .run(move || {
                let table = onnx::load(
                    &bytes,
                    entry.vocabulary_size,
                    entry.dimension,
                    signal.as_ref(),
                )?;
                let mut tokenizer = Tokenizer::from_bytes(tokenizer).map_err(|e| {
                    ModelError::storage_failure("Invalid Bekko tokenizer").with_cause(e)
                })?;
                tokenizer.with_padding(None);
                // A single pass retains one extra token for the truncation flag.
                // No special tokens or prefixes; valid UNK IDs remain in the input.
                tokenizer
                    .with_truncation(Some(TruncationParams {
                        max_length: entry.max_input_tokens.checked_add(1).ok_or_else(|| {
                            ModelError::invalid_argument("Invalid Bekko token limit")
                        })?,
                        direction: TruncationDirection::Right,
                        ..TruncationParams::default()
                    }))
                    .map_err(|e| {
                        ModelError::storage_failure("Invalid Bekko tokenizer truncation")
                            .with_cause(e)
                    })?;
                check_cancelled(signal.as_ref())?;
                Ok::<_, ModelError>(Loaded { tokenizer, table })
            })
            .await??;
        reporter.finish();
        Ok(loaded)
    }
}

#[async_trait]
impl EmbeddingModel for BekkoEmbeddingModel {
    fn info(&self) -> &EmbeddingModelInfo {
        &self.info
    }

    async fn prepare(&self, options: EmbeddingPrepareOptions) -> Result<(), ModelError> {
        self.ensure_loaded(options.on_progress, options.signal.as_ref())
            .await
            .map(|_| ())
            .map_err(ModelError::shared)
    }

    async fn embed(
        &self,
        inputs: &[Vec<Content>],
        options: EmbeddingOptions,
    ) -> Result<EmbeddingResult, ModelError> {
        validate_inputs(&self.info, inputs)?;
        let loaded = self
            .ensure_loaded(options.on_progress, options.signal.as_ref())
            .await
            .map_err(ModelError::shared)?;
        let texts = inputs
            .iter()
            .map(|input| input_text(input).map(std::borrow::Cow::into_owned))
            .collect::<Result<Vec<_>, _>>()?;
        let limit = self.entry.max_input_tokens;
        let signal = options.signal;
        let result = self
            .compute
            .run(move || {
                batch::embed_texts(
                    &texts,
                    &loaded.tokenizer,
                    &loaded.table,
                    limit,
                    signal.as_ref(),
                )
            })
            .await??;
        validate_result(&self.info, inputs.len(), &result)?;
        Ok(result)
    }
}

#[cfg(test)]
#[path = "model/tests.rs"]
mod tests;
