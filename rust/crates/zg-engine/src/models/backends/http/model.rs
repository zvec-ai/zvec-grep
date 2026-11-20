use std::{fmt, time::Duration};

use async_trait::async_trait;
use reqwest::{Url, header::HeaderValue};
use serde_json::{Value, json};

use crate::{
    EngineError,
    domain::{
        Content,
        model::{
            EmbeddingModelInfo, EmbeddingPurpose, EmbeddingResult, HttpAuth, HttpEmbeddingConfig,
            ModelConfig,
        },
    },
    models::spi::{EmbeddingModel, EmbeddingOptions, ModelError, input_text, validate_inputs},
};

use super::{response::parse_response, transport::HttpTransport};

pub(crate) struct HttpEmbeddingModel {
    info: EmbeddingModelInfo,
    descriptor: HttpEmbeddingConfig,
    transport: HttpTransport,
}

impl fmt::Debug for HttpEmbeddingModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpEmbeddingModel")
            .finish_non_exhaustive()
    }
}

impl HttpEmbeddingModel {
    pub(crate) fn new(reference: &str, options: ModelConfig) -> Result<Self, ModelError> {
        let descriptor = options.http.ok_or_else(|| {
            ModelError::invalid_argument("HTTP embedding requires a descriptor").shared()
        })?;
        // Share the prefix reservation with metadata resolution and persisted model info.
        let info = descriptor.model_info(reference).map_err(|_| {
            ModelError::invalid_argument("Invalid HTTP embedding descriptor or reference").shared()
        })?;
        let endpoint = options.endpoint.as_deref().ok_or_else(invalid_endpoint)?;
        let endpoint = crate::authorization::canonical_remote_endpoint(reference, endpoint)
            .map_err(|_| invalid_endpoint())?;
        let endpoint = Url::parse(&endpoint).map_err(|_| invalid_endpoint())?;
        let timeout = Duration::from_millis(descriptor.timeout_ms);
        if std::time::Instant::now().checked_add(timeout).is_none() {
            return Err(
                ModelError::invalid_argument("HTTP embedding timeout is too large").shared(),
            );
        }
        let authorization = match descriptor.auth {
            HttpAuth::None => None,
            HttpAuth::Bearer => {
                let name = descriptor.api_key_env.as_deref().ok_or_else(missing_key)?;
                let key = std::env::var(name).map_err(|_| missing_key())?;
                if key.trim().is_empty() {
                    return Err(missing_key());
                }
                let mut header =
                    HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| missing_key())?;
                header.set_sensitive(true);
                Some(header)
            }
        };
        Ok(Self {
            info,
            descriptor,
            transport: HttpTransport::new(endpoint, authorization, timeout)?,
        })
    }
}

#[async_trait]
impl EmbeddingModel for HttpEmbeddingModel {
    fn info(&self) -> &EmbeddingModelInfo {
        &self.info
    }

    async fn embed(
        &self,
        inputs: &[Vec<Content>],
        options: EmbeddingOptions,
    ) -> Result<EmbeddingResult, ModelError> {
        if options
            .signal
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            return Err(ModelError::cancelled(
                "HTTP embedding request was cancelled",
            ));
        }
        validate_inputs(&self.info, inputs)?;
        let prefix = match options.purpose {
            EmbeddingPurpose::Query => &self.descriptor.query_prefix,
            EmbeddingPurpose::Document => &self.descriptor.document_prefix,
        };
        let texts = inputs
            .iter()
            .map(|input| {
                let text = input_text(input)?;
                Ok(format!("{prefix}{text}"))
            })
            .collect::<Result<Vec<_>, ModelError>>()?;
        let input =
            if self.descriptor.single_input {
                Value::String(texts.into_iter().next().ok_or_else(|| {
                    ModelError::invalid_argument("HTTP embedding requires an input")
                })?)
            } else {
                json!(texts)
            };
        let mut request = json!({ "model": self.descriptor.model, "input": input });
        if self.descriptor.send_dimensions {
            request["dimensions"] = json!(self.descriptor.dimension);
        }
        if self.descriptor.send_encoding_format {
            request["encoding_format"] = json!("float");
        }
        let bytes = self
            .transport
            .post(
                &request,
                options.signal.as_ref(),
                options.trace_headers.as_ref(),
            )
            .await?;
        parse_response(&bytes, &self.info, inputs.len())
    }
}

fn invalid_endpoint() -> ModelError {
    ModelError::invalid_argument(
        "HTTP embedding requires an HTTP(S) endpoint without credentials, query, or fragment",
    )
    .shared()
}

fn missing_key() -> ModelError {
    ModelError::new(
        EngineError::PERMISSION_DENIED,
        "HTTP embedding bearer authentication requires a valid key in its named environment variable",
        None,
    ).shared()
}
