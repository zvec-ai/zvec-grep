//! User-approved first indexing with request-bound interaction continuations.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use rmcp::{
    ErrorData, RoleServer,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ElicitationAction,
        InputRequiredResult, ProtocolVersion,
    },
    service::RequestContext,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zg_engine::{
    EngineError, ZvecGrep,
    api::{
        context::ContextOptions,
        index::{
            IndexOptions,
            options::EmbeddingModelSpec,
            setup::{IndexSetup, IndexSetupOptions},
        },
        info::{InfoOptions, result::WorkspaceIndexPolicy},
    },
    authorization::{self, IndexAuthorization},
};

use crate::{
    IndexOperationProvider, IndexOperationState, consent, error_result, structured_result,
};

#[derive(Clone)]
pub(crate) struct SetupCall(pub(crate) CallToolRequestParams);

pub(crate) fn is_setup_state(request: &CallToolRequestParams) -> bool {
    request
        .request_state
        .as_deref()
        .is_some_and(|state| state.starts_with("zg-setup-"))
}

#[derive(Default)]
pub(crate) struct SetupState {
    pending: Mutex<HashMap<String, Pending>>,
    builds: Mutex<HashMap<PathBuf, String>>,
}

struct Pending {
    expires: Instant,
    binding: Vec<u8>,
    phase: Phase,
}

#[derive(Clone)]
enum Phase {
    Model {
        root: PathBuf,
        setup: IndexSetup,
    },
    Authorization {
        options: Box<IndexOptions>,
        target: IndexAuthorization,
        setup: IndexSetup,
    },
}

impl SetupState {
    pub(crate) fn is_continuation(context: &RequestContext<RoleServer>) -> bool {
        context
            .extensions
            .get::<SetupCall>()
            .is_some_and(|call| is_setup_state(&call.0))
    }

    pub(crate) fn running(
        &self,
        root: Option<&Path>,
        provider: &dyn IndexOperationProvider,
    ) -> Option<CallToolResult> {
        let requested = std::fs::canonicalize(root?).ok()?;
        let mut builds = self.builds.lock().ok()?;
        let tracked = builds
            .keys()
            .filter(|root| requested.starts_with(root))
            .max_by_key(|root| root.components().count())
            .cloned();
        // An initial CLI build has no published manifest either. Follow its job
        // without waiting on the writer or asking for a second model selection.
        let root = tracked.or_else(|| {
            requested
                .ancestors()
                .find(|root| {
                    !root.join(".zvec-grep/manifest.json").is_file()
                        && provider.runtime_snapshot(root).is_some_and(|snapshot| {
                            matches!(
                                snapshot.job_state,
                                Some(IndexOperationState::Queued | IndexOperationState::Running)
                            )
                        })
                })
                .map(Path::to_path_buf)
        })?;
        if requested
            .ancestors()
            .take_while(|path| *path != root)
            .any(|path| path.join(".zvec-grep/manifest.json").is_file())
        {
            return None;
        }
        let snapshot = provider.runtime_snapshot(&root);
        let job_id = builds.get(&root).cloned().or_else(|| {
            snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.active_job_id.clone())
        })?;
        if let Some(snapshot) = snapshot
            && snapshot.active_job_id.as_deref() == Some(&job_id)
        {
            if matches!(
                snapshot.job_state,
                Some(IndexOperationState::Queued | IndexOperationState::Running)
            ) {
                return Some(structured_result(json!({
                    "status": "indexing", "root": zg_cli::output_path(&root), "job_id": job_id,
                    "state": if snapshot.job_state == Some(IndexOperationState::Queued) { "queued" } else { "running" },
                    "progress": snapshot.progress,
                    "message": "The initial index is still building. No search was performed. Retry the original search after indexing completes."
                })));
            }
            if let Some(error) = snapshot.error {
                builds.remove(&root);
                return Some(structured_result(
                    json!({"status": "indexing_failed", "root": zg_cli::output_path(&root),
                    "job_id": job_id, "error": error.report,
                    "message": "The index build failed. Retry the search to review model setup again."}),
                ));
            }
        }
        builds.remove(&root);
        None
    }

    pub(crate) async fn start(
        &self,
        engine: &ZvecGrep,
        provider: &dyn IndexOperationProvider,
        request: ContextOptions,
        context: &RequestContext<RoleServer>,
        original_error: EngineError,
    ) -> Result<CallToolResponse, ErrorData> {
        let result = async {
            let info = provider.info(engine, InfoOptions { root: request.root.clone(), include_status: false }).await?;
            info.compatibility.ensure_compatible()?;
            if info.indexed {
                // A model/provider NOT_FOUND must not become an index creation prompt.
                return Err(original_error);
            }
            if info.index_policy == WorkspaceIndexPolicy::Disabled {
                return Err(EngineError::not_found("Workspace indexing is disabled; index creation requires an explicit zg --index command"));
            }
            if matches!(info.compatibility, zg_engine::api::info::result::IndexCompatibility::Compatible { .. }) {
                return Err(EngineError::storage_failure("Index storage is missing; rebuild it with `zg --index --rebuild`"));
            }
            if !provider.supports_background_indexing() || !form_supported(context) {
                return Err(EngineError::not_found(format!(
                    "No index found in {}. This MCP connection cannot request interactive background indexing. Run `zg --index --embedding <model>` in the workspace, then retry the search. No index was created.", zg_cli::output_path(&info.root).display()
                )));
            }
            let setup = engine.index_setup(IndexSetupOptions {
                model_cache: request.model_cache.clone(), ..Default::default()
            }).await?;
            self.drive(engine, provider, &request, context, Phase::Model { root: info.root, setup }, None).await
        }.await;
        Ok(result.unwrap_or_else(|error| error_result(&error).into()))
    }

    pub(crate) async fn resume(
        &self,
        engine: &ZvecGrep,
        provider: &dyn IndexOperationProvider,
        request: ContextOptions,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let invalid =
            || ErrorData::invalid_params("Invalid or expired index setup requestState", None);
        let call = context.extensions.get::<SetupCall>().ok_or_else(invalid)?;
        let token = call.0.request_state.as_deref().ok_or_else(invalid)?;
        let pending = {
            let mut states = self.pending.lock().map_err(|_| invalid())?;
            states.retain(|_, state| state.expires > Instant::now());
            if states
                .get(token)
                .is_none_or(|state| state.binding != binding(&call.0, context))
            {
                return Err(invalid());
            }
            states.remove(token).ok_or_else(invalid)?
        };
        let response = call
            .0
            .input_responses
            .as_ref()
            .and_then(|responses| responses.get("index_setup"))
            .cloned()
            .ok_or_else(invalid)?;
        if context.ct.is_cancelled() {
            return Ok(error_result(&EngineError::cancelled("Index setup cancelled")).into());
        }
        Ok(self
            .drive(
                engine,
                provider,
                &request,
                context,
                pending.phase,
                Some(response),
            )
            .await
            .unwrap_or_else(|error| error_result(&error).into()))
    }

    async fn drive(
        &self,
        engine: &ZvecGrep,
        provider: &dyn IndexOperationProvider,
        request: &ContextOptions,
        context: &RequestContext<RoleServer>,
        mut phase: Phase,
        mut response: Option<Value>,
    ) -> Result<CallToolResponse, EngineError> {
        loop {
            if context.ct.is_cancelled() {
                return Err(EngineError::cancelled("Index setup cancelled"));
            }
            let (message, schema) = form(&phase, context.protocol_version().as_ref());
            let answer = if let Some(response) = response.take() {
                accepted_content(&response)?
            } else if context
                .protocol_version()
                .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28)
            {
                return self.prompt(phase, &message, &schema, context);
            } else {
                let result = consent::elicit_form(message, schema, context).await?;
                if result.action != ElicitationAction::Accept {
                    return Err(EngineError::cancelled(
                        "Index setup cancelled; no index was created",
                    ));
                }
                result
                    .content
                    .ok_or_else(|| EngineError::invalid_argument("Missing index setup response"))?
            };
            match phase {
                Phase::Model { root, setup } => {
                    let reference =
                        answer.get("model").and_then(Value::as_str).ok_or_else(|| {
                            EngineError::invalid_argument("Select a supported embedding model")
                        })?;
                    let choice = setup
                        .models
                        .iter()
                        .find(|model| model.reference == reference && model.available())
                        .ok_or_else(|| {
                            EngineError::invalid_argument("Selected embedding model is unavailable")
                        })?;
                    let options = IndexOptions {
                        root: Some(root),
                        embedding: Some(EmbeddingModelSpec {
                            reference: reference.to_owned(),
                            revision: None,
                            cache_dir: None,
                            endpoint: None,
                            device: zg_engine::api::index::options::Device::Auto,
                        }),
                        model_cache: request.model_cache.clone(),
                        device: if choice.local { request.device } else { None },
                        embedding_concurrency: request.embedding_concurrency,
                        ..Default::default()
                    };
                    if let Some(target) = authorization::index_authorization(&options)? {
                        phase = Phase::Authorization {
                            options: Box::new(options),
                            target,
                            setup,
                        };
                    } else {
                        return self.submit(provider, options, context).await;
                    }
                }
                Phase::Authorization {
                    options,
                    target,
                    setup,
                } => {
                    if answer.get("decision").and_then(Value::as_str) == Some("back") {
                        phase = Phase::Model {
                            root: target.root,
                            setup,
                        };
                        continue;
                    }
                    let options = Self::approve(engine, *options, target, &answer).await?;
                    return self.submit(provider, options, context).await;
                }
            }
        }
    }

    async fn approve(
        engine: &ZvecGrep,
        mut options: IndexOptions,
        target: IndexAuthorization,
        answer: &Value,
    ) -> Result<IndexOptions, EngineError> {
        match answer.get("decision").and_then(Value::as_str) {
            Some("allow_once") => options.authorized_remote.push(target.clone()),
            Some("allow_workspace") => {
                if authorization::index_authorization(&options)?.as_ref() != Some(&target) {
                    return Err(EngineError::permission_denied(
                        "Remote destination changed; retry model setup",
                    ));
                }
                authorization::grant_index(&target)?;
            }
            _ => {
                return Err(EngineError::cancelled(
                    "Index setup cancelled; no remote data was sent",
                ));
            }
        }
        if authorization::index_authorization(&options)?.is_some() {
            return Err(EngineError::permission_denied(
                "Remote destination changed; retry model setup",
            ));
        }
        // The model may have been selected in an earlier interaction round.
        let current = engine.index_setup(IndexSetupOptions::default()).await?;
        if !current
            .models
            .iter()
            .any(|model| model.reference == target.model && model.available())
        {
            return Err(EngineError::invalid_argument(
                "Selected model no longer has an available credential",
            ));
        }
        Ok(options)
    }

    fn prompt(
        &self,
        phase: Phase,
        message: &str,
        schema: &Value,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, EngineError> {
        let call = context
            .extensions
            .get::<SetupCall>()
            .ok_or_else(|| EngineError::internal("Missing index setup request"))?;
        let mut states = self
            .pending
            .lock()
            .map_err(|_| EngineError::internal("Index setup unavailable"))?;
        states.retain(|_, state| state.expires > Instant::now());
        if states.len() >= 4096 {
            return Err(EngineError::internal(
                "Too many pending index setup requests",
            ));
        }
        let token = format!(
            "zg-setup-{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        states.insert(
            token.clone(),
            Pending {
                expires: Instant::now() + Duration::from_secs(600),
                binding: binding(&call.0, context),
                phase,
            },
        );
        let requests = serde_json::from_value(
            json!({"index_setup": {"method": "elicitation/create", "params": {
                "mode": "form", "message": message, "requestedSchema": schema
            }}}),
        )
        .map_err(|error| EngineError::internal(error.to_string()))?;
        Ok(InputRequiredResult::new(Some(requests), Some(token)).into())
    }

    async fn submit(
        &self,
        provider: &dyn IndexOperationProvider,
        mut options: IndexOptions,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, EngineError> {
        if context.ct.is_cancelled() {
            return Err(EngineError::cancelled("Index setup cancelled"));
        }
        // The accepted background build outlives this search request.
        options.signal = None;
        let result = provider.submit_index(options, false).await?;
        if result.error.is_some() {
            return Ok(crate::index_operation_to_result(&result, false).into());
        }
        let mut builds = self
            .builds
            .lock()
            .map_err(|_| EngineError::internal("Index task status unavailable"))?;
        builds.retain(|root, _| provider.runtime_snapshot(root).is_some());
        if builds.len() >= 4096 {
            builds.clear();
        }
        builds.insert(result.root.clone(), result.job_id.clone());
        Ok(structured_result(json!({
            "status": if result.state == IndexOperationState::Succeeded { "ready" } else { "indexing" },
            "root": zg_cli::output_path(&result.root), "job_id": result.job_id, "reused": result.reused,
            "message": "Index creation was authorized and submitted. No search was performed. Retry the original search after indexing completes."
        })).into())
    }
}

fn form_supported(context: &RequestContext<RoleServer>) -> bool {
    context
        .client_capabilities()
        .and_then(|caps| caps.elicitation)
        .is_some_and(|cap| cap.form.is_some() || cap.url.is_none())
}

fn binding(request: &CallToolRequestParams, context: &RequestContext<RoleServer>) -> Vec<u8> {
    let principal = context
        .extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.headers.get(http::header::AUTHORIZATION))
        .map_or(&[][..], |value| value.as_bytes());
    let mut binding = json!([request.name, request.arguments, principal]);
    // Object key order is not part of request identity; array order and values are.
    binding.sort_all_objects();
    Sha256::digest(binding.to_string().as_bytes()).to_vec()
}

fn accepted_content(response: &Value) -> Result<Value, EngineError> {
    if response["action"] != "accept" {
        return Err(EngineError::cancelled(
            "Index setup cancelled; no index was created",
        ));
    }
    response
        .get("content")
        .filter(|value| value.is_object())
        .cloned()
        .ok_or_else(|| EngineError::invalid_argument("Invalid index setup response"))
}

fn choice_property(
    title: &str,
    choices: &[Value],
    default: Option<&str>,
    version: Option<&ProtocolVersion>,
) -> Value {
    let mut property = json!({"type": "string", "title": title});
    if version.is_some_and(|version| *version >= ProtocolVersion::V_2025_11_25) {
        property["oneOf"] = json!(choices);
    } else {
        property["enum"] = choices
            .iter()
            .map(|choice| choice["const"].clone())
            .collect();
        property["enumNames"] = choices
            .iter()
            .map(|choice| choice["title"].clone())
            .collect();
    }
    if let Some(default) = default {
        property["default"] = json!(default);
    }
    property
}

fn form(phase: &Phase, version: Option<&ProtocolVersion>) -> (String, Value) {
    match phase {
        Phase::Model { root, setup } => {
            let choices = setup.models.iter().filter(|model| model.available()).map(|model| json!({
                "const": model.reference,
                "title": format!("{} · {} · {}", if model.local { "Local" } else { "Remote" }, model.reference,
                    if model.local { if model.downloaded { "Downloaded" } else { "Download required" } }
                    else { "Credential available" })
            })).collect::<Vec<_>>();
            let default = setup
                .default_model
                .as_ref()
                .filter(|reference| {
                    setup
                        .models
                        .iter()
                        .any(|model| &model.reference == *reference && model.available())
                })
                .cloned()
                .or_else(|| {
                    setup
                        .models
                        .iter()
                        .find(|model| model.available())
                        .map(|model| model.reference.clone())
                });
            (
                format!(
                    "No index found in {}. Select an embedding model to create a persistent index in the background. Local models run on this server. Remote models require a separate authorization step. Cancel to exit without creating an index.",
                    zg_cli::output_path(root).display()
                ),
                json!({"type": "object", "properties": {"model": choice_property("Embedding model", &choices, default.as_deref(), version)}, "required": ["model"]}),
            )
        }
        Phase::Authorization { target, .. } => (
            format!(
                "Allow remote embedding? Workspace: {}. Model: {}. Destination: {}. Sends selected workspace files. API charges may apply. Allow once covers this background build only; a later query requires its own authorization. Workspace approval also authorizes later matching queries and refreshes. No workspace content has been sent. Cancel to exit.",
                zg_cli::output_path(&target.root).display(),
                target.model,
                target.endpoint
            ),
            json!({"type": "object", "properties": {"decision": choice_property("Remote embedding authorization", &[
                json!({"const": "allow_once", "title": "Allow once"}),
                json!({"const": "allow_workspace", "title": "Allow for this workspace"}),
                json!({"const": "back", "title": "Back to model selection"})
            ], None, version)}, "required": ["decision"]}),
        ),
    }
}

#[cfg(test)]
mod tests;
