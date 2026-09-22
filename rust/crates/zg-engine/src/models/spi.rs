//! Backend contract shared by the model runtime and embedding implementations.

use std::{borrow::Cow, collections::HashSet, fmt, sync::Arc};

use crate::domain::Content;
use crate::domain::model::{EmbeddingModelInfo, EmbeddingPurpose, EmbeddingResult, ModelProgress};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub(super) use super::error::ModelError;

pub(crate) fn input_text(input: &[Content]) -> Result<Cow<'_, str>, ModelError> {
    match input {
        [] => Err(ModelError::invalid_argument(
            "Embedding input requires at least one content item",
        )),
        [Content::Text(text)] => Ok(Cow::Borrowed(text)),
        contents => {
            let mut combined = String::new();
            for (index, content) in contents.iter().enumerate() {
                let Content::Text(text) = content else {
                    return Err(ModelError::unsupported(
                        "Text embedding requires text content",
                    ));
                };
                if index > 0 {
                    combined.push('\n');
                }
                combined.push_str(text);
            }
            Ok(Cow::Owned(combined))
        }
    }
}

/// Observes model preparation together with the operation's effective concurrency.
#[derive(Clone)]
pub(crate) struct ModelProgressReporter(Arc<dyn Fn(ModelProgress, usize) + Send + Sync + 'static>);

impl ModelProgressReporter {
    pub(crate) fn new(reporter: impl Fn(ModelProgress, usize) + Send + Sync + 'static) -> Self {
        Self(Arc::new(reporter))
    }

    pub(super) fn report(&self, progress: ModelProgress, concurrency: usize) {
        (self.0)(progress, concurrency);
    }
}

#[derive(Clone, Default)]
pub struct EmbeddingOptions {
    pub purpose: EmbeddingPurpose,
    pub signal: Option<CancellationToken>,
    pub on_progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
    /// Runtime-owned execution budget. Backends use this to size their
    /// per-request local inference resources without exposing another public
    /// tuning knob.
    pub(crate) execution_concurrency: usize,
    /// Standard W3C trace headers propagated by remote embedding backends.
    pub(crate) trace_headers: Option<EmbeddingTraceHeaders>,
}

#[derive(Clone, Default)]
pub(crate) struct EmbeddingPrepareOptions {
    pub(crate) signal: Option<CancellationToken>,
    pub(crate) on_progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct EmbeddingTraceHeaders {
    pub(crate) traceparent: String,
    pub(crate) tracestate: Option<String>,
    pub(crate) baggage: Option<String>,
}

impl fmt::Debug for EmbeddingOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EmbeddingOptions")
            .field("purpose", &self.purpose)
            .field("has_signal", &self.signal.is_some())
            .field("has_progress_callback", &self.on_progress.is_some())
            .field("execution_concurrency", &self.execution_concurrency)
            .field("has_trace_headers", &self.trace_headers.is_some())
            .finish()
    }
}

/// Backend defaults used by runtime admission and the indexing scheduler.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EmbeddingConcurrencyDefaults {
    pub initial: usize,
    pub maximum: usize,
}

impl Default for EmbeddingConcurrencyDefaults {
    fn default() -> Self {
        Self {
            initial: 1,
            maximum: 1,
        }
    }
}

#[async_trait]
pub trait EmbeddingModel: Send + Sync {
    fn info(&self) -> &EmbeddingModelInfo;

    fn concurrency_defaults(&self) -> EmbeddingConcurrencyDefaults {
        EmbeddingConcurrencyDefaults::default()
    }

    /// Loads shared local resources before an operation dispatches embedding work.
    /// Remote and already-ready models may keep the default no-op implementation.
    async fn prepare(&self, _options: EmbeddingPrepareOptions) -> Result<(), ModelError> {
        Ok(())
    }

    /// Embeds a batch of inputs, producing one vector per inner content list.
    /// Content items within an input are ordered; each backend validates supported combinations.
    async fn embed(
        &self,
        inputs: &[Vec<Content>],
        options: EmbeddingOptions,
    ) -> Result<EmbeddingResult, ModelError>;
}

pub(crate) fn validate_inputs(
    info: &EmbeddingModelInfo,
    inputs: &[Vec<Content>],
    accepts: impl Fn(&Content) -> bool,
) -> Result<(), ModelError> {
    info.validate().map_err(|error| {
        ModelError::internal("Embedding model returned invalid metadata")
            .with_cause(error)
            .shared()
    })?;
    if inputs.is_empty() {
        return Err(ModelError::new(
            crate::EngineError::INVALID_ARGUMENT,
            "Embedding requires at least one input",
            None,
        ));
    }
    if inputs.len() > info.max_batch_size {
        return Err(ModelError::new(
            crate::EngineError::INVALID_ARGUMENT,
            "Embedding batch size exceeds model limit",
            Some(format!(
                "model={} batchSize={} maxBatchSize={}",
                info.model.reference(),
                inputs.len(),
                info.max_batch_size
            )),
        ));
    }

    for (index, input) in inputs.iter().enumerate() {
        validate_input(info, index, input, &accepts)?;
    }
    Ok(())
}

fn validate_input(
    info: &EmbeddingModelInfo,
    index: usize,
    input: &[Content],
    accepts: &impl Fn(&Content) -> bool,
) -> Result<(), ModelError> {
    if input.is_empty() {
        return Err(ModelError::new(
            crate::EngineError::INVALID_ARGUMENT,
            "Embedding input requires at least one content item",
            Some(format!(
                "model={} inputIndex={index}",
                info.model.reference()
            )),
        ));
    }
    for (part_index, content) in input.iter().enumerate() {
        if !accepts(content) {
            return Err(ModelError::new(
                crate::EngineError::UNSUPPORTED,
                "Embedding model does not support content",
                Some(format!(
                    "model={} inputIndex={index} partIndex={part_index}",
                    info.model.reference()
                )),
            ));
        }

        match content {
            Content::Text(text) if text.trim().is_empty() => {
                return Err(ModelError::new(
                    crate::EngineError::INVALID_ARGUMENT,
                    "Embedding text content must not be empty",
                    Some(format!(
                        "model={} inputIndex={index} partIndex={part_index}",
                        info.model.reference()
                    )),
                ));
            }
            Content::Image(image)
                if info
                    .max_image_bytes
                    .is_some_and(|maximum| image.data().len() > maximum) =>
            {
                let maximum = info.max_image_bytes.unwrap_or_default();
                return Err(ModelError::new(
                    crate::EngineError::INVALID_ARGUMENT,
                    "Embedding image content exceeds model limit",
                    Some(format!(
                        "model={} inputIndex={index} partIndex={part_index} imageBytes={} maxImageBytes={maximum}",
                        info.model.reference(),
                        image.data().len()
                    )),
                ));
            }
            Content::Text(_) | Content::Image(_) | Content::Table(_) => {}
        }
    }
    Ok(())
}

pub(crate) fn validate_result(
    info: &EmbeddingModelInfo,
    input_count: usize,
    result: &EmbeddingResult,
) -> Result<(), ModelError> {
    if result.vectors.len() != input_count {
        return Err(ModelError::new(
            crate::EngineError::INTERNAL,
            "Embedding model returned the wrong number of vectors",
            Some(format!(
                "model={} inputCount={input_count} vectorCount={}",
                info.model.reference(),
                result.vectors.len()
            )),
        ));
    }
    for (vector_index, vector) in result.vectors.iter().enumerate() {
        if vector.len() != info.dimension {
            return Err(ModelError::new(
                crate::EngineError::INTERNAL,
                "Embedding model returned a vector with the wrong dimension",
                Some(format!(
                    "model={} vectorIndex={vector_index} expectedDimension={} actualDimension={}",
                    info.model.reference(),
                    info.dimension,
                    vector.len()
                )),
            )
            .shared());
        }
        if let Some(value_index) = vector.iter().position(|value| !value.is_finite()) {
            return Err(ModelError::new(
                crate::EngineError::INTERNAL,
                "Embedding model returned a non-finite vector value",
                Some(format!(
                    "model={} vectorIndex={vector_index} valueIndex={value_index}",
                    info.model.reference()
                )),
            ));
        }
    }

    let mut seen = HashSet::new();
    for &index in &result.truncated {
        if index >= input_count || !seen.insert(index) {
            return Err(ModelError::new(
                crate::EngineError::INTERNAL,
                "Embedding model returned an invalid truncated input index",
                Some(format!(
                    "model={} index={index} inputCount={input_count}",
                    info.model.reference()
                )),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::domain::{Content, FileFormat, ImageContent, TableContent, model::Metric};

    use super::*;

    fn validate_inputs(
        info: &EmbeddingModelInfo,
        inputs: &[Vec<Content>],
    ) -> Result<(), ModelError> {
        super::validate_inputs(info, inputs, |content| {
            matches!(content, Content::Text(_) | Content::Image(_))
        })
    }

    #[test]
    fn invalid_model_info_is_not_reported_as_a_bad_embedding_request() {
        let mut info = fixture_info();
        info.max_batch_size = 0;
        let error = validate_inputs(&info, &[vec![Content::Text("valid input".into())]])
            .expect_err("model metadata must be checked before request limits");
        assert_eq!(error.code(), crate::EngineError::INTERNAL);
        assert!(
            error
                .cause()
                .expect("validation detail")
                .contains("max_batch_size")
        );
    }

    #[test]
    fn validates_all_representable_input_failures_from_the_typescript_base_class() {
        let info = fixture_info();

        assert_error_code(
            validate_inputs(&info, &[]),
            crate::EngineError::INVALID_ARGUMENT,
        );
        assert_error_code(
            validate_inputs(
                &info,
                &[
                    vec![Content::Text("one".to_owned())],
                    vec![Content::Text("two".to_owned())],
                    vec![Content::Text("three".to_owned())],
                ],
            ),
            crate::EngineError::INVALID_ARGUMENT,
        );
        assert_error_code(
            validate_inputs(&info, &[vec![Content::Text("  ".to_owned())]]),
            crate::EngineError::INVALID_ARGUMENT,
        );
        assert_error_code(
            validate_inputs(&info, &[Vec::new()]),
            crate::EngineError::INVALID_ARGUMENT,
        );
        assert_eq!(
            ImageContent::new(Vec::new(), FileFormat::Png)
                .expect_err("empty image must be rejected before embedding")
                .code(),
            crate::EngineError::INVALID_ARGUMENT,
        );
        assert_error_code(
            validate_inputs(
                &info,
                &[vec![Content::Image(
                    ImageContent::new(vec![1, 2, 3, 4], FileFormat::Png).expect("image"),
                )]],
            ),
            crate::EngineError::INVALID_ARGUMENT,
        );

        assert_error_code(
            super::validate_inputs(
                &info,
                &[vec![Content::Image(
                    ImageContent::new(vec![1], FileFormat::Png).expect("image"),
                )]],
                |content| matches!(content, Content::Text(_)),
            ),
            crate::EngineError::UNSUPPORTED,
        );
        assert_error_code(
            validate_inputs(
                &info,
                &[vec![Content::Table(TableContent {
                    row_count: 0,
                    column_count: 0,
                    cells: Vec::new(),
                })]],
            ),
            crate::EngineError::UNSUPPORTED,
        );
    }

    #[test]
    fn preserves_input_boundaries_when_combining_text_parts() {
        let mut info = fixture_info();
        info.max_batch_size = 1;
        let input = vec![
            Content::Text("first".to_owned()),
            Content::Text("second".to_owned()),
            Content::Text("third".to_owned()),
        ];
        validate_inputs(&info, std::slice::from_ref(&input)).expect("one input");
        assert_eq!(input_text(&input).expect("text"), "first\nsecond\nthird");
        assert!(matches!(
            input_text(&[Content::Text("one".into())]).expect("text"),
            Cow::Borrowed("one")
        ));
        let image = ImageContent::new(vec![1], FileFormat::Png).expect("image");
        let mixed = vec![Content::Text("first".to_owned()), Content::Image(image)];
        assert_eq!(
            input_text(&mixed).expect_err("non-text part").code(),
            crate::EngineError::UNSUPPORTED,
        );
    }

    #[test]
    fn validates_all_representable_provider_output_failures() {
        let info = fixture_info();

        assert_error_code(
            validate_result(
                &info,
                1,
                &EmbeddingResult {
                    vectors: Vec::new(),
                    truncated: Vec::new(),
                },
            ),
            crate::EngineError::INTERNAL,
        );
        assert_error_code(
            validate_result(
                &info,
                1,
                &EmbeddingResult {
                    vectors: vec![vec![1.0]],
                    truncated: Vec::new(),
                },
            ),
            crate::EngineError::INTERNAL,
        );
        assert_error_code(
            validate_result(
                &info,
                1,
                &EmbeddingResult {
                    vectors: vec![vec![1.0, f32::NAN]],
                    truncated: Vec::new(),
                },
            ),
            crate::EngineError::INTERNAL,
        );
        for truncated in [vec![1], vec![0, 0]] {
            assert_error_code(
                validate_result(
                    &info,
                    1,
                    &EmbeddingResult {
                        vectors: vec![vec![1.0, 0.0]],
                        truncated,
                    },
                ),
                crate::EngineError::INTERNAL,
            );
        }
    }

    fn fixture_info() -> EmbeddingModelInfo {
        EmbeddingModelInfo {
            model: crate::domain::model::ModelInfo {
                provider: "test".into(),
                name: "stub".into(),
                endpoint: None,
            },
            dimension: 2,
            metric: Metric::Cosine,
            max_batch_size: 2,
            max_input_tokens: None,
            max_image_bytes: Some(3),
        }
    }

    fn assert_error_code(result: Result<(), ModelError>, expected: &'static str) {
        assert_eq!(
            result
                .expect_err("validation should reject the fixture")
                .code(),
            expected
        );
    }
}
