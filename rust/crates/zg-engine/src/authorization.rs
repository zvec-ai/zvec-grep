//! Signed workspace consent shared by direct, daemon, and MCP operations.
//!
//! Consent preflight and grant-file management are the explicit exception to
//! the `ZvecGrep` operation boundary described in `rust/CONTRIBUTING.md`.
//! They operate before engine work and never load models or send remote data.

use crate::{
    EngineError,
    models::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry},
    utils::{atomic_write, sync_directory},
    workspace::{
        layout::{find_nearest_workspace, workspace_index_location},
        manifest::{WorkspaceManifest, inspect_workspace_manifest, read_workspace_manifest},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// Resolved workspace and destination disclosed before an index operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IndexAuthorization {
    pub root: PathBuf,
    pub workspace_roots: Vec<PathBuf>,
    pub model: String,
    pub endpoint: String,
    pub endpoint_host: String,
}

/// Verified workspace consent, without terminal presentation details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizationStatus {
    pub root: PathBuf,
    pub path: PathBuf,
    pub grants: Vec<AuthorizationGrantStatus>,
}

/// A verified remote destination approved for a workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizationGrantStatus {
    pub model: String,
    pub endpoint: String,
    pub endpoint_host: String,
}

/// Resolves required consent without loading models, writing state, or sending data.
///
/// # Errors
/// Returns errors for invalid workspace state, models, endpoints, or signed grants.
pub fn index_authorization(
    options: &crate::api::index::IndexOptions,
) -> Result<Option<IndexAuthorization>, EngineError> {
    Ok(index_authorizations(options)?.into_iter().next())
}

/// Resolve the remote destination used by the workspace embedding model.
/// # Errors
/// Returns invalid configuration, workspace or authorization errors.
pub fn index_authorizations(
    options: &crate::api::index::IndexOptions,
) -> Result<Vec<IndexAuthorization>, EngineError> {
    let requested_root = crate::workspace::layout::resolve_workspace_root(options.root.as_deref())?;
    let location = match find_nearest_workspace(&requested_root)? {
        Some(location) => location,
        None => workspace_index_location(&requested_root)?,
    };
    let existing = inspect_workspace_manifest(&location.home)?.into_manifest(options.rebuild)?;
    authorizations_for_manifest(options, &location.root, existing.as_ref())
}

fn authorizations_for_manifest(
    options: &crate::api::index::IndexOptions,
    root: &Path,
    existing: Option<&WorkspaceManifest>,
) -> Result<Vec<IndexAuthorization>, EngineError> {
    crate::pipelines::indexing::service::embedding_plan(existing, options)?
        .requests
        .values()
        .filter_map(|request| authorization_for_manifest(request, root, existing).transpose())
        .collect()
}

fn authorization_for_manifest(
    options: &crate::api::index::IndexOptions,
    workspace_root: &Path,
    existing: Option<&WorkspaceManifest>,
) -> Result<Option<IndexAuthorization>, EngineError> {
    let model = crate::pipelines::indexing::service::embedding_reference(
        existing,
        options.embedding.as_ref(),
    )?;
    if options.allow_remote || model.starts_with("local/") {
        return Ok(None);
    }
    let endpoint = remote_endpoint(
        &model,
        options
            .endpoint
            .as_deref()
            .or_else(|| {
                options
                    .embedding
                    .as_ref()
                    .and_then(|e| e.endpoint.as_deref())
            })
            .or_else(|| {
                existing
                    .and_then(|m| m.embedding_runtimes.get(&model))
                    .and_then(|runtime| runtime.endpoint.as_deref())
            }),
    )?;
    let root = fs::canonicalize(workspace_root).map_err(io)?;
    if approved_destination(&root, &model, &endpoint, &options.authorized_remote)
        || read_grants(&root)?
            .iter()
            .any(|g| g.model == model && g.endpoint == endpoint)
    {
        return Ok(None);
    }
    let url = reqwest::Url::parse(&endpoint)
        .map_err(|_| EngineError::invalid_argument("Invalid embedding endpoint"))?;
    let endpoint_host = format_endpoint_host(&url);
    let workspace_roots = vec![root.clone()];
    Ok(Some(IndexAuthorization {
        root,
        workspace_roots,
        model,
        endpoint,
        endpoint_host,
    }))
}

fn format_endpoint_host(url: &reqwest::Url) -> String {
    match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    }
}

/// Destination and data categories requiring consent for an indexed query.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueryAuthorization {
    pub target: IndexAuthorization,
    pub query_text: bool,
    pub query_image: bool,
    pub workspace_content: bool,
}

/// Resolves query consent without embedding text or updating the index.
/// # Errors
/// Returns invalid request, workspace, endpoint, or signed grant errors.
pub fn query_authorization(
    options: &crate::api::context::ContextOptions,
) -> Result<Option<QueryAuthorization>, EngineError> {
    Ok(query_authorizations(options)?.into_iter().next())
}

/// Resolve the query and optional refresh destination before sending data.
/// # Errors
/// Returns invalid request, workspace or consent errors.
pub fn query_authorizations(
    options: &crate::api::context::ContextOptions,
) -> Result<Vec<QueryAuthorization>, EngineError> {
    use crate::api::context::options::{ContextRouteMode, RefreshPolicy};
    if options.rg {
        return Ok(Vec::new());
    }
    let request = crate::pipelines::indexed_search::context::normalize_context_request(options)?;
    let vector = request
        .routes
        .iter()
        .any(|route| route.mode == ContextRouteMode::Vector);
    let workspace_content = options
        .refresh
        .map_or(options.auto_update, |refresh| refresh != RefreshPolicy::Off);
    let root = crate::workspace::layout::resolve_workspace_root(options.root.as_deref())?;
    let Some(location) = find_nearest_workspace(&root)? else {
        return Ok(Vec::new());
    };
    let Some(manifest) = read_workspace_manifest(&location.home)? else {
        return Ok(Vec::new());
    };
    let Some(descriptor) = manifest.workspace.index.descriptor() else {
        return Ok(Vec::new());
    };
    if options.allow_remote || (!vector && !workspace_content) {
        return Ok(Vec::new());
    }
    let base = crate::api::index::IndexOptions {
        root: Some(location.root.clone()),
        authorized_remote: options.authorized_remote.clone(),
        ..crate::api::index::IndexOptions::default()
    };
    let mut plans: Vec<QueryAuthorization> = if workspace_content {
        authorizations_for_manifest(&base, &location.root, Some(&manifest))?
            .into_iter()
            .map(|target| QueryAuthorization {
                target,
                query_text: false,
                query_image: false,
                workspace_content: true,
            })
            .collect()
    } else {
        Vec::new()
    };
    if vector {
        let kind = options.input_kind();
        let schema = descriptor.model_for(kind)?.ok_or_else(|| {
            EngineError::unsupported(format!(
                "workspace has no embedding model supporting {} queries",
                kind.as_str(),
            ))
        })?;
        let query = crate::api::index::IndexOptions {
            endpoint: options.endpoint.clone(),
            embedding: Some(crate::api::index::options::EmbeddingModelSpec {
                reference: schema.model.reference(),
                revision: None,
                cache_dir: None,
                endpoint: None,
                device: crate::domain::model::Device::Auto,
            }),
            ..base
        };
        if let Some(target) = authorization_for_manifest(&query, &location.root, Some(&manifest))? {
            let image = kind == crate::domain::ContentKind::Image;
            if let Some(plan) = plans.iter_mut().find(|plan| plan.target == target) {
                plan.query_text = !image;
                plan.query_image = image;
            } else {
                plans.push(QueryAuthorization {
                    target,
                    query_text: !image,
                    query_image: image,
                    workspace_content: false,
                });
            }
        }
    }
    Ok(plans)
}

/// Persists a destination explicitly approved by the terminal user.
///
/// # Errors
/// Returns errors for invalid destinations or inaccessible signing state.
pub fn grant_index(target: &IndexAuthorization) -> Result<(), EngineError> {
    let root = fs::canonicalize(&target.root).map_err(io)?;
    let endpoint = remote_endpoint(&target.model, Some(&target.endpoint))?;
    persist_grant(Grant {
        version: 1,
        root,
        capability: "embedding".into(),
        scope: "workspace".into(),
        model: target.model.clone(),
        endpoint,
    })
}

#[derive(Serialize, Deserialize)]
struct Grant {
    version: u32,
    root: PathBuf,
    capability: String,
    scope: String,
    model: String,
    endpoint: String,
}

#[derive(Serialize, Deserialize)]
struct SignedGrant {
    grant: Grant,
    signature: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SignedGrants {
    One(SignedGrant),
    Many(Vec<SignedGrant>),
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "used directly by Result::map_err"
)]
fn io(error: std::io::Error) -> EngineError {
    EngineError::from_io("workspace authorization", &error)
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "used directly by Result::map_err"
)]
fn json(error: serde_json::Error) -> EngineError {
    EngineError::invalid_argument(format!("Invalid workspace authorization: {error}"))
}

fn root_path(root: &Path) -> Result<PathBuf, EngineError> {
    let root = fs::canonicalize(root).map_err(io)?;
    if !root.is_dir() {
        return Err(EngineError::invalid_argument(
            "Workspace root must be a directory",
        ));
    }
    Ok(find_nearest_workspace(&root)?.map_or(root, |location| location.root))
}

fn key_path() -> Result<PathBuf, EngineError> {
    if let Some(path) = env::var_os("ZVEC_GREP_AUTHORIZATION_KEY_FILE") {
        return Ok(path.into());
    }
    let home = env::var_os("ZVEC_GREP_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".zvec-grep")))
        .ok_or_else(|| {
            EngineError::invalid_argument("Set ZVEC_GREP_AUTHORIZATION_KEY_FILE or HOME")
        })?;
    Ok(home.join("authorization.key"))
}

fn private_write(path: &Path, data: &[u8]) -> Result<(), EngineError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(io)?;
    file.write_all(data).map_err(io)?;
    file.sync_all().map_err(io)
}

fn signing_key(create: bool) -> Result<Vec<u8>, EngineError> {
    let path = key_path()?;
    if create && !path.exists() {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut builder = fs::DirBuilder::new();
        builder.recursive(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        if let Err(error) = builder.create(parent)
            && !(error.kind() == std::io::ErrorKind::AlreadyExists && parent.is_dir())
        {
            return Err(EngineError::from_io(
                format!(
                    "create authorization key directory '{}' (parent must already exist)",
                    parent.display()
                ),
                &error,
            ));
        }
        sync_directory(parent)?;
        if parent.file_name().is_some() {
            sync_directory(parent.parent().unwrap_or(parent))?;
        }
        let temporary = parent.join(format!(".authorization-key-{}", uuid::Uuid::new_v4()));
        let key = [
            uuid::Uuid::new_v4().as_bytes().as_slice(),
            uuid::Uuid::new_v4().as_bytes().as_slice(),
        ]
        .concat();
        private_write(&temporary, &key)?;
        let result = fs::hard_link(&temporary, &path);
        let _ = fs::remove_file(&temporary);
        if let Err(error) = result
            && error.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(io(error));
        }
        sync_directory(parent)?;
    }
    let key = fs::read(path).map_err(io)?;
    if key.len() < 32 {
        return Err(EngineError::invalid_argument(
            "Authorization signing key must contain at least 32 bytes",
        ));
    }
    Ok(key)
}

pub(crate) fn remote_endpoint(
    reference: &str,
    endpoint: Option<&str>,
) -> Result<String, EngineError> {
    let Some(EmbeddingCatalogEntry::Qwen(entry)) = get_embedding_model_catalog_entry(reference)
    else {
        return Err(EngineError::invalid_argument(
            "Authorization requires a supported remote embedding model",
        ));
    };
    let config = crate::config::read()?;
    let endpoint = endpoint
        .map(str::to_owned)
        .or_else(|| crate::config::string(&config, &["models", reference, "endpoint"]))
        .or_else(|| env::var("ZVEC_GREP_ENDPOINT").ok())
        .unwrap_or_else(|| entry.default_endpoint.to_string());
    let mut url = reqwest::Url::parse(endpoint.trim())
        .map_err(|_| EngineError::invalid_argument("Invalid remote embedding endpoint"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(EngineError::invalid_argument(
            "Embedding endpoint must be an HTTP(S) URL without credentials or fragment",
        ));
    }
    // SDK examples provide an API base URL. Resolve it before consent is
    // checked so the signed destination and the actual request URL agree.
    let path = url.path().trim_end_matches('/');
    if entry.kind == "text" && path.ends_with("/v1") {
        url.set_path(&format!("{path}/embeddings"));
    }
    Ok(url.to_string())
}

/// Persists explicit workspace consent without loading a model or contacting a provider.
///
/// # Errors
/// Returns an error for invalid models, roots, or inaccessible signing state.
pub fn grant(
    root: &Path,
    model: Option<&str>,
    endpoint: Option<&str>,
) -> Result<String, EngineError> {
    let root = root_path(root)?;
    let location = workspace_index_location(&root)?;
    let manifest = inspect_workspace_manifest(&location.home)?.into_manifest(true)?;
    let config = crate::config::read()?;
    let model = model
        .map(str::to_owned)
        .or_else(|| {
            manifest
                .as_ref()
                .and_then(|m| m.default_embedding())
                .map(|e| e.model.reference())
        })
        .or_else(|| {
            env::var("ZVEC_GREP_EMBEDDING")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| crate::config::string(&config, &["defaults", "embedding"]))
        .ok_or_else(|| {
            EngineError::invalid_argument(
                "Choose a remote model with --embedding or ZVEC_GREP_EMBEDDING",
            )
        })?;
    let endpoint = remote_endpoint(
        &model,
        endpoint.or_else(|| {
            manifest
                .as_ref()
                .and_then(|m| m.embedding_runtimes.get(&model))
                .and_then(|runtime| runtime.endpoint.as_deref())
        }),
    )?;
    let grant = Grant {
        version: 1,
        root: root.clone(),
        capability: "embedding".into(),
        scope: "workspace".into(),
        model,
        endpoint,
    };
    persist_grant(grant)?;
    status(&root)
}

fn persist_grant(grant: Grant) -> Result<(), EngineError> {
    let location = workspace_index_location(&grant.root)?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    if let Err(error) = builder.create(&location.home)
        && !(error.kind() == std::io::ErrorKind::AlreadyExists && location.home.is_dir())
    {
        return Err(EngineError::from_io(
            format!(
                "create workspace authorization directory '{}'",
                location.home.display()
            ),
            &error,
        ));
    }
    let _lock = crate::workspace::lock::acquire_read_write_lock(
        &location.home.join("authorization.lock"),
        crate::workspace::lock::LockMode::Write,
        "authorization.grant",
    )?;
    let mut grants = read_grants(&grant.root)?;
    grants.retain(|previous| previous.model != grant.model || previous.endpoint != grant.endpoint);
    grants.push(grant);
    let key = signing_key(true)?;
    let signed = grants
        .into_iter()
        .map(|grant| {
            let signature =
                hmac_sha256::HMAC::mac(serde_json::to_vec(&grant).map_err(json)?, &key).to_vec();
            Ok(SignedGrant { grant, signature })
        })
        .collect::<Result<Vec<_>, EngineError>>()?;
    let bytes = if signed.len() == 1 {
        serde_json::to_vec_pretty(&signed[0])
    } else {
        serde_json::to_vec_pretty(&signed)
    }
    .map_err(json)?;
    atomic_write(&location.home.join("authorization.json"), &bytes)?;
    sync_directory(&location.root)
}

fn read_grants(root: &Path) -> Result<Vec<Grant>, EngineError> {
    let path = root.join(".zvec-grep/authorization.json");
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(e)),
    };
    let records: SignedGrants = serde_json::from_slice(&bytes).map_err(|error| {
        EngineError::invalid_argument(format!(
            "Invalid workspace authorization format: {error}; run `zg --auth revoke \"{}\"`, then authorize again with `zg --auth grant`",
            root.display()
        ))
    })?;
    let records = match records {
        SignedGrants::One(record) => vec![record],
        SignedGrants::Many(records) => records,
    };
    records
        .into_iter()
        .map(|signed| {
            let expected = hmac_sha256::HMAC::mac(
                serde_json::to_vec(&signed.grant).map_err(json)?,
                signing_key(false)?,
            );
            let valid = signed.signature.len() == expected.len()
                && signed
                    .signature
                    .iter()
                    .zip(expected)
                    .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
                    == 0;
            if !valid
                || signed.grant.version != 1
                || signed.grant.root != root
                || signed.grant.capability != "embedding"
                || signed.grant.scope != "workspace"
            {
                return Err(EngineError::permission_denied(format!(
                    "Invalid workspace authorization signature or scope; run `zg --auth revoke \"{}\"`, then authorize again with `zg --auth grant`",
                    root.display()
                )));
            }
            Ok(signed.grant)
        })
        .collect()
}

/// Reports verified consent without creating any state.
///
/// # Errors
/// Returns an error if existing consent cannot be verified.
pub fn status_snapshot(root: &Path) -> Result<AuthorizationStatus, EngineError> {
    let root = root_path(root)?;
    let grants = read_grants(&root)?
        .into_iter()
        .map(|grant| {
            let endpoint_host = reqwest::Url::parse(&grant.endpoint)
                .map_or_else(|_| grant.endpoint.clone(), |url| format_endpoint_host(&url));
            AuthorizationGrantStatus {
                model: grant.model,
                endpoint: grant.endpoint,
                endpoint_host,
            }
        })
        .collect();
    Ok(AuthorizationStatus {
        path: root.join(".zvec-grep/authorization.json"),
        root,
        grants,
    })
}

/// Reports verified consent without creating any state.
///
/// # Errors
/// Returns an error if existing consent cannot be verified.
pub fn status(root: &Path) -> Result<String, EngineError> {
    let status = status_snapshot(root)?;
    if status.grants.is_empty() {
        return Ok(format!(
            "Remote Embedding: not authorized\nRoot: {}",
            status.root.display()
        ));
    }
    let destinations = status
        .grants
        .iter()
        .map(|grant| format!("Model: {}\nEndpoint: {}", grant.model, grant.endpoint))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!(
        "Remote Embedding: authorized\nRoot: {}\nScope: workspace\n{destinations}",
        status.root.display()
    ))
}

/// Removes workspace consent. Repeated revocations are harmless.
///
/// # Errors
/// Returns an error when the authorization file cannot be removed or its deletion synced.
pub fn revoke(root: &Path) -> Result<String, EngineError> {
    let root = root_path(root)?;
    revoke_all(&root)?;
    Ok(format!(
        "Remote Embedding: not authorized\nRoot: {}",
        root.display()
    ))
}

/// Removes all workspace grants and reports how many records were removed.
/// Invalid consent can still be removed without a working signing key.
/// Returns `None` when removed consent could not be read or parsed to count records.
///
/// # Errors
/// Returns an error when authorization state cannot be removed or synced.
pub fn revoke_all(root: &Path) -> Result<Option<usize>, EngineError> {
    let root = root_path(root)?;
    let home = root.join(".zvec-grep");
    let path = home.join("authorization.json");
    let count = match fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<SignedGrants>(&bytes) {
            Ok(SignedGrants::Many(records)) => Some(records.len()),
            Ok(SignedGrants::One(_)) => Some(1),
            // Explicit revocation remains the recovery path for malformed consent.
            Err(_) => None,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Some(0)),
        // Revocation must also work when existing consent cannot be read.
        Err(_) => None,
    };
    match fs::remove_file(path) {
        Ok(()) => sync_directory(&home)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Some(0)),
        Err(e) => return Err(io(e)),
    }
    Ok(count)
}

fn approved_destination(
    root: &Path,
    model: &str,
    endpoint: &str,
    targets: &[IndexAuthorization],
) -> bool {
    targets
        .iter()
        .any(|target| target.root == root && target.model == model && target.endpoint == endpoint)
}

pub(crate) fn require_with_targets(
    root: &Path,
    model: &str,
    endpoint: &str,
    once: bool,
    targets: &[IndexAuthorization],
) -> Result<(), EngineError> {
    let root = fs::canonicalize(root).map_err(io)?;
    require(
        &root,
        model,
        endpoint,
        once || approved_destination(&root, model, endpoint, targets),
    )
}

pub(crate) fn require(
    root: &Path,
    model: &str,
    endpoint: &str,
    once: bool,
) -> Result<(), EngineError> {
    if once {
        return Ok(());
    }
    let root = fs::canonicalize(root).map_err(io)?;
    if read_grants(&root)?
        .iter()
        .any(|g| g.model == model && g.endpoint == endpoint)
    {
        return Ok(());
    }
    Err(EngineError::permission_denied(format!(
        "Remote embedding authorization required for {model} at {endpoint}. Run zg --auth grant \"{}\" --capability embedding --scope workspace --embedding {model} --endpoint \"{endpoint}\", or pass --allow-remote for this command only.",
        root.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::index::{IndexOptions, options::EmbeddingModelSpec},
        domain::model::Device,
    };

    fn isolated_authorization_root(test_name: &str) -> Option<PathBuf> {
        const FIXTURE_ENV: &str = "ZG_AUTHORIZATION_RECOVERY_TEST_ROOT";
        if let Some(root) = env::var_os(FIXTURE_ENV) {
            return Some(PathBuf::from(root));
        }
        // Child-process configuration keeps signing keys and global settings isolated
        // without changing environment variables while other unit tests are running.
        let workspace = tempfile::tempdir().expect("workspace");
        let state = tempfile::tempdir().expect("authorization state");
        let output = std::process::Command::new(env::current_exe().expect("test executable"))
            .args(["--exact", test_name, "--nocapture"])
            .env(FIXTURE_ENV, workspace.path())
            .env("HOME", state.path())
            .env("USERPROFILE", state.path())
            .env("ZVEC_GREP_AUTHORIZATION_KEY_FILE", state.path().join("key"))
            .env_remove("ZVEC_GREP_EMBEDDING")
            .env_remove("ZVEC_GREP_ENDPOINT")
            .output()
            .expect("isolated authorization test");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        None
    }

    #[test]
    fn text_embedding_base_urls_resolve_before_authorization() {
        let Some(_root) = isolated_authorization_root(
            "authorization::tests::text_embedding_base_urls_resolve_before_authorization",
        ) else {
            return;
        };
        for model in ["qwen/text-embedding-v4", "qwen/qwen3.7-text-embedding"] {
            for (input, expected) in [
                ("/v1", "/v1/embeddings"),
                ("/v1/", "/v1/embeddings"),
                ("/compatible-mode/v1", "/compatible-mode/v1/embeddings"),
                ("/compatible-mode/v1/", "/compatible-mode/v1/embeddings"),
                (
                    "/gateway/v1/?api-version=2026-01",
                    "/gateway/v1/embeddings?api-version=2026-01",
                ),
                ("/v1/embeddings", "/v1/embeddings"),
                ("/custom-embeddings", "/custom-embeddings"),
                ("/v10", "/v10"),
            ] {
                let expected = format!("https://example.test{expected}");
                let resolved =
                    remote_endpoint(model, Some(&format!(" https://example.test{input} ")))
                        .expect("valid text endpoint");
                assert_eq!(resolved, expected);
                assert_eq!(
                    remote_endpoint(model, Some(&resolved)).expect("idempotent endpoint"),
                    expected
                );
            }
        }
        assert_eq!(
            remote_endpoint("qwen/qwen3-vl-embedding", Some("https://example.test/v1/"))
                .expect("multimodal endpoint"),
            "https://example.test/v1/"
        );
        for endpoint in [
            "ftp://example.test/v1",
            "https://user:password@example.test/v1",
            "https://example.test/v1#fragment",
        ] {
            assert!(remote_endpoint("qwen/qwen3.7-text-embedding", Some(endpoint)).is_err());
        }
    }

    #[test]
    fn legacy_or_corrupt_authorization_requires_revoke_before_regrant() {
        let Some(root) = isolated_authorization_root(
            "authorization::tests::legacy_or_corrupt_authorization_requires_revoke_before_regrant",
        ) else {
            return;
        };
        let home = root.join(".zvec-grep");
        fs::create_dir(&home).expect("workspace home");
        let path = home.join("authorization.json");
        let legacy = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "grants": [{
                "version": 1,
                "id": "node-grant",
                "capability": "embedding",
                "scope": "workspace",
                "workspaceRoots": [root],
                "workspaceFingerprint": "node-workspace",
                "provider": "qwen",
                "model": "text-embedding-v4",
                "endpoint": "https://provider.test/embeddings",
                "targetFingerprint": "node-target",
                "grantedAt": 1,
                "signature": "0123456789abcdef"
            }]
        }))
        .expect("Node authorization fixture");
        for bytes in [legacy, b"{broken authorization".to_vec()] {
            fs::write(&path, &bytes).expect("authorization fixture");
            let options = IndexOptions {
                root: Some(root.clone()),
                rebuild: true,
                embedding: Some(EmbeddingModelSpec {
                    reference: "qwen/text-embedding-v4".into(),
                    revision: None,
                    cache_dir: None,
                    endpoint: Some("https://provider.test/embeddings".into()),
                    device: Device::Auto,
                }),
                ..IndexOptions::default()
            };
            let error = index_authorizations(&options).expect_err("old consent is not trusted");
            assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
            assert!(error.message().contains("zg --auth revoke"));
            assert!(error.message().contains("zg --auth grant"));
            assert!(
                grant(
                    &root,
                    Some("qwen/text-embedding-v4"),
                    Some("https://provider.test/embeddings")
                )
                .is_err()
            );
            assert_eq!(fs::read(&path).expect("preserved consent"), bytes);
            revoke(&root).expect("explicitly revoke incompatible consent");
            assert!(!path.exists());
            grant(
                &root,
                Some("qwen/text-embedding-v4"),
                Some("https://provider.test/embeddings"),
            )
            .expect("grant after revoke");
            assert!(
                index_authorizations(&options)
                    .expect("new grant is valid")
                    .is_empty()
            );
        }
    }

    #[test]
    fn explicit_grant_works_before_rebuilding_a_node_index() {
        let Some(root) = isolated_authorization_root(
            "authorization::tests::explicit_grant_works_before_rebuilding_a_node_index",
        ) else {
            return;
        };
        let home = root.join(".zvec-grep");
        fs::create_dir(&home).expect("workspace home");
        let path = home.join("manifest.json");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "manifestVersion": 1,
            "indexVersion": 1,
            "id": "node-index",
            "name": "workspace",
            "path": home,
            "rootPaths": [{ "absolutePath": root, "recursive": true }],
            "indexPolicy": "enabled",
            "embedding": { "provider": "qwen", "model": "text-embedding-v3", "dimension": 1024, "metric": "cosine" },
            "createdTime": 1,
            "updatedTime": 1,
            "embeddingRuntime": {}
        })).expect("Node manifest fixture");
        fs::write(&path, &bytes).expect("old manifest");
        grant(
            &root,
            Some("qwen/text-embedding-v4"),
            Some("https://provider.test/embeddings"),
        )
        .expect("explicit authorization before rebuild");
        let grants = read_grants(&fs::canonicalize(&root).expect("canonical root"))
            .expect("verified grants");
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].model, "qwen/text-embedding-v4");
        assert_eq!(grants[0].endpoint, "https://provider.test/embeddings");
        assert_eq!(fs::read(&path).expect("unchanged manifest"), bytes);
    }

    #[test]
    fn invalid_signature_or_scope_explains_revoke_before_regrant() {
        let Some(root) = isolated_authorization_root(
            "authorization::tests::invalid_signature_or_scope_explains_revoke_before_regrant",
        ) else {
            return;
        };
        let root = fs::canonicalize(root).expect("canonical root");
        let path = root.join(".zvec-grep/authorization.json");
        for invalid_scope in [false, true] {
            grant(
                &root,
                Some("qwen/text-embedding-v4"),
                Some("https://provider.test/embeddings"),
            )
            .expect("valid authorization");
            let mut signed: SignedGrant =
                serde_json::from_slice(&fs::read(&path).expect("grant")).expect("signed grant");
            if invalid_scope {
                signed.grant.scope = "once".into();
                signed.signature = hmac_sha256::HMAC::mac(
                    serde_json::to_vec(&signed.grant).expect("grant JSON"),
                    signing_key(false).expect("test signing key"),
                )
                .to_vec();
            } else {
                signed.signature[0] ^= 1;
            }
            let bytes = serde_json::to_vec(&signed).expect("invalid authorization fixture");
            fs::write(&path, &bytes).expect("invalid authorization");
            let error = read_grants(&root)
                .err()
                .expect("reject invalid authorization");
            assert_eq!(error.code(), EngineError::PERMISSION_DENIED);
            assert!(error.message().contains("zg --auth revoke"));
            assert!(error.message().contains("zg --auth grant"));
            assert_eq!(fs::read(&path).expect("preserved invalid consent"), bytes);
            revoke(&root).expect("revoke invalid authorization");
        }
    }

    #[test]
    fn single_model_discloses_one_remote_destination() {
        let directory = tempfile::tempdir().expect("workspace");
        let spec = |reference: &str, endpoint: &str| EmbeddingModelSpec {
            reference: reference.into(),
            revision: None,
            cache_dir: None,
            endpoint: Some(endpoint.into()),
            device: Device::Auto,
        };
        let options = IndexOptions {
            root: Some(directory.path().into()),
            embedding: Some(spec(
                "qwen/text-embedding-v4",
                "https://text.example.test/embeddings",
            )),
            ..IndexOptions::default()
        };
        let targets = index_authorizations(&options).expect("destination");
        assert_eq!(targets.len(), 1);
        assert!(
            targets
                .iter()
                .any(|target| target.model == "qwen/text-embedding-v4"
                    && target.endpoint_host == "text.example.test")
        );
        assert!(
            !directory.path().join(".zvec-grep").exists(),
            "disclosure is read-only"
        );
        let approved = IndexOptions {
            authorized_remote: targets.clone(),
            ..options.clone()
        };
        assert!(
            index_authorizations(&approved)
                .expect("approved destinations")
                .is_empty()
        );
        let target = &targets[0];
        require_with_targets(
            directory.path(),
            &target.model,
            &target.endpoint,
            false,
            &targets,
        )
        .expect("exact approved destination");
        let other_root = tempfile::tempdir().expect("different workspace");
        for (root, model, endpoint) in [
            (
                directory.path(),
                target.model.as_str(),
                "https://changed.example.test/embeddings",
            ),
            (
                directory.path(),
                "qwen/unapproved-model",
                target.endpoint.as_str(),
            ),
            (
                other_root.path(),
                target.model.as_str(),
                target.endpoint.as_str(),
            ),
        ] {
            assert!(require_with_targets(root, model, endpoint, false, &targets).is_err());
        }
        let once = IndexOptions {
            allow_remote: true,
            ..options
        };
        assert!(
            index_authorizations(&once)
                .expect("explicit invocation grant")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn query_disclosure_uses_index_destination_and_actual_refresh_policy() {
        use crate::api::context::{
            ContextOptions,
            options::{ContextRoute, ContextRouteMode, RefreshPolicy},
        };
        let directory = tempfile::tempdir().expect("workspace");
        let engine = crate::ZvecGrep::new();
        engine
            .index(IndexOptions {
                root: Some(directory.path().into()),
                allow_remote: true,
                api_key: Some("test-key".into()),
                embedding: Some(EmbeddingModelSpec {
                    reference: "qwen/text-embedding-v4".into(),
                    revision: None,
                    cache_dir: None,
                    endpoint: None,
                    device: Device::Auto,
                }),
                endpoint: Some("https://query.test/embeddings".into()),
                ..IndexOptions::default()
            })
            .await
            .expect("empty index without network");
        engine.close();
        let manifest = directory.path().join(".zvec-grep/manifest.json");
        let before = fs::read(&manifest).expect("manifest");
        let child = directory.path().join("docs");
        fs::create_dir(&child).expect("child directory");
        let mut request = ContextOptions {
            root: Some(child),
            query: Some("bookstore".into()),
            refresh: Some(RefreshPolicy::Off),
            ..ContextOptions::default()
        };
        let plan = query_authorization(&request)
            .expect("preflight")
            .expect("consent");
        assert!(plan.query_text && !plan.workspace_content);
        assert_eq!(
            plan.target.root,
            fs::canonicalize(directory.path()).expect("root")
        );
        assert_eq!(plan.target.endpoint_host, "query.test");
        assert_eq!(fs::read(&manifest).expect("manifest"), before);
        assert!(
            !directory
                .path()
                .join(".zvec-grep/authorization.json")
                .exists()
        );
        request.refresh = Some(RefreshPolicy::Background);
        assert!(
            query_authorization(&request)
                .expect("background")
                .expect("consent")
                .workspace_content
        );
        request.query = None;
        request.routes = vec![ContextRoute {
            mode: ContextRouteMode::Fts,
            query: "bookstore".into(),
        }];
        request.refresh = Some(RefreshPolicy::Off);
        assert!(query_authorization(&request).expect("FTS").is_none());
        request.refresh = Some(RefreshPolicy::Wait);
        let plan = query_authorization(&request)
            .expect("wait")
            .expect("refresh consent");
        assert!(!plan.query_text && plan.workspace_content);
        request.allow_remote = true;
        assert!(
            query_authorization(&request)
                .expect("explicit consent")
                .is_none()
        );
    }

    #[test]
    fn abandoned_build_does_not_override_active_authorization() {
        use crate::{
            api::context::{ContextOptions, options::RefreshPolicy},
            domain::{
                IndexDescriptor, IndexState, Workspace,
                model::{EmbeddingMetric, EmbeddingModelInfo, ModelConfig},
            },
            workspace::{build::prepare_build, manifest::write_workspace_manifest},
        };
        let directory = tempfile::tempdir().expect("workspace");
        let home = directory.path().join(".zvec-grep");
        let mut active = WorkspaceManifest::new(
            Workspace {
                name: "workspace".to_owned(),
                root: directory.path().to_path_buf(),
                scan: crate::domain::ScanRules::default(),
                index: IndexState::Enabled(IndexDescriptor::single(EmbeddingModelInfo {
                    model: crate::domain::model::ModelInfo::new(
                        "qwen",
                        "text-embedding-v4",
                        [
                            crate::domain::ContentKind::Text,
                            crate::domain::ContentKind::Code,
                        ],
                    )
                    .expect("fixture model identity"),
                    dimension: 1024,
                    metric: EmbeddingMetric::Cosine,
                    max_batch_size: 32,
                    max_input_tokens: None,
                    max_image_bytes: None,
                })),
                created_epoch_ms: 1,
                updated_epoch_ms: 1,
            },
            home.clone(),
            Some(crate::workspace::CURRENT_INDEX_VERSION),
            std::collections::BTreeMap::from([(
                "qwen/text-embedding-v4".into(),
                ModelConfig {
                    endpoint: Some("https://active.test/embeddings".into()),
                    ..ModelConfig::default()
                },
            )]),
        )
        .expect("manifest");
        active.storage_generation = Some(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(active.storage_home()).expect("active generation");
        write_workspace_manifest(&home, &active).expect("write active");
        let mut target = active.clone();
        target
            .embedding_runtimes
            .values_mut()
            .next()
            .expect("runtime")
            .endpoint = Some("https://staging.test/embeddings".into());
        prepare_build(target).expect("staged build");
        let index = index_authorization(&IndexOptions {
            root: Some(directory.path().into()),
            ..IndexOptions::default()
        })
        .expect("index disclosure")
        .expect("remote index");
        assert_eq!(index.endpoint_host, "active.test");
        let query = query_authorization(&ContextOptions {
            root: Some(directory.path().into()),
            query: Some("query".into()),
            refresh: Some(RefreshPolicy::Wait),
            ..ContextOptions::default()
        })
        .expect("query disclosure")
        .expect("remote query");
        assert_eq!(query.target.endpoint_host, "active.test");
        assert!(query.query_text);
        assert!(query.workspace_content);

        // An abandoned initial build must not silently select its remote model.
        fs::remove_file(home.join("manifest.json")).expect("unpublished workspace");
        let child = directory.path().join("src/nested");
        fs::create_dir_all(&child).expect("nested source directory");
        assert!(
            index_authorization(&IndexOptions {
                root: Some(child),
                ..IndexOptions::default()
            })
            .expect("default local model")
            .is_none()
        );
    }

    #[test]
    fn index_disclosure_matches_destination_without_creating_state() {
        let directory = tempfile::tempdir().expect("workspace");
        let mut options = IndexOptions {
            root: Some(directory.path().into()),
            embedding: Some(EmbeddingModelSpec {
                reference: "qwen/text-embedding-v4".into(),
                revision: None,
                cache_dir: None,
                endpoint: Some("https://provider.test:8443/v1/embeddings".into()),
                device: Device::Auto,
            }),
            ..IndexOptions::default()
        };
        let target = index_authorization(&options)
            .expect("plan")
            .expect("remote consent");
        assert_eq!(target.endpoint_host, "provider.test:8443");
        assert_eq!(target.workspace_roots, vec![target.root.clone()]);
        assert_eq!(target.model, "qwen/text-embedding-v4");
        assert!(!directory.path().join(".zvec-grep").exists());
        options.endpoint = Some("https://override.test/embeddings".into());
        assert_eq!(
            index_authorization(&options)
                .expect("override")
                .expect("consent")
                .endpoint_host,
            "override.test"
        );
        options.allow_remote = true;
        assert!(index_authorization(&options).expect("once").is_none());
        options.allow_remote = false;
        options.embedding.as_mut().expect("model").reference = "local/potion-code-16m-v2".into();
        assert!(index_authorization(&options).expect("local").is_none());
    }

    #[test]
    fn indexing_discloses_each_routed_destination_and_validates_capabilities() {
        use crate::domain::ContentKind;
        let directory = tempfile::tempdir().expect("workspace");
        let spec = |reference: &str, endpoint: &str| EmbeddingModelSpec {
            reference: reference.into(),
            endpoint: Some(endpoint.into()),
            revision: None,
            cache_dir: None,
            device: Device::Auto,
        };
        let mut options = IndexOptions {
            root: Some(directory.path().into()),
            embedding: Some(spec(
                "qwen/text-embedding-v4",
                "https://text.test/embeddings",
            )),
            embedding_routes: std::collections::BTreeMap::from([(
                ContentKind::Image,
                spec("qwen/qwen3-vl-embedding", "https://image.test/embeddings"),
            )]),
            ..IndexOptions::default()
        };
        let targets = index_authorizations(&options).expect("destinations");
        assert_eq!(targets.len(), 2);
        assert!(
            targets
                .iter()
                .any(|target| target.endpoint_host == "text.test")
        );
        assert!(
            targets
                .iter()
                .any(|target| target.endpoint_host == "image.test")
        );
        assert!(!directory.path().join(".zvec-grep").exists());
        options.authorized_remote = targets;
        assert!(
            index_authorizations(&options)
                .expect("approved destinations")
                .is_empty()
        );
        options.embedding_routes.insert(
            ContentKind::Image,
            spec("qwen/text-embedding-v4", "https://text.test/embeddings"),
        );
        assert!(index_authorizations(&options).is_err());
    }

    async fn routed_query_authorization_fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("workspace");
        let engine = crate::ZvecGrep::new();
        engine
            .index(IndexOptions {
                root: Some(directory.path().into()),
                allow_remote: true,
                api_key: Some("test-key".into()),
                embedding: Some(EmbeddingModelSpec {
                    reference: "qwen/qwen3-vl-embedding".into(),
                    endpoint: Some("https://images.test/embeddings".into()),
                    revision: None,
                    cache_dir: None,
                    device: Device::Auto,
                }),
                ..IndexOptions::default()
            })
            .await
            .expect("empty multimodal index");
        engine.close();
        let path = directory.path().join(".zvec-grep/manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("authorization fixture"))
                .expect("authorization fixture");
        manifest["embeddings"].as_array_mut().expect("authorization fixture").push(serde_json::json!({
            "model": {"provider":"qwen", "name":"text-embedding-v4", "contentKinds":["text","code"]},
            "dimension":1024, "metric":"cosine", "maxBatchSize":10, "maxInputTokens":8192,
            "maxImageBytes":null
        }));
        manifest["embeddingRoutes"] = serde_json::json!({"text":"qwen/text-embedding-v4"});
        manifest["embeddingRuntimes"]["qwen/text-embedding-v4"] =
            serde_json::json!({"endpoint":"https://text.test/embeddings"});
        fs::write(
            &path,
            serde_json::to_vec(&manifest).expect("authorization fixture"),
        )
        .expect("authorization fixture");
        directory
    }

    #[tokio::test]
    async fn query_and_refresh_disclose_only_their_actual_destinations_and_content() {
        use crate::api::context::{
            ContextOptions,
            options::{QueryImage, RefreshPolicy},
        };
        let directory = routed_query_authorization_fixture().await;
        let mut options = ContextOptions {
            root: Some(directory.path().into()),
            query: Some("text query".into()),
            refresh: Some(RefreshPolicy::Off),
            ..ContextOptions::default()
        };
        let targets = query_authorizations(&options).expect("text query");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target.endpoint_host, "text.test");
        assert!(targets[0].query_text && !targets[0].query_image && !targets[0].workspace_content);
        options.refresh = Some(RefreshPolicy::Wait);
        options.endpoint = Some("https://query-override.test/embeddings".into());
        let targets = query_authorizations(&options).expect("query plus refresh");
        assert_eq!(targets.len(), 3);
        assert!(
            targets
                .iter()
                .filter(|target| target.workspace_content)
                .all(|target| !target.query_text && !target.query_image)
        );
        assert_eq!(
            targets
                .iter()
                .find(|target| target.query_text)
                .expect("authorization fixture")
                .target
                .endpoint_host,
            "query-override.test"
        );
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("authorization fixture");
        options.query = None;
        options.query_image = Some(QueryImage::Bytes {
            format: crate::domain::FileFormat::Png,
            data: png.into_inner(),
        });
        options.endpoint = None;
        options.refresh = Some(RefreshPolicy::Off);
        let targets = query_authorizations(&options).expect("image query");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target.endpoint_host, "images.test");
        assert!(!targets[0].query_text && targets[0].query_image && !targets[0].workspace_content);
    }

    #[test]
    fn disclosure_and_status_use_the_same_endpoint_host() {
        let Some(root) = isolated_authorization_root(
            "authorization::tests::disclosure_and_status_use_the_same_endpoint_host",
        ) else {
            return;
        };
        for (endpoint, expected) in [
            ("https://provider.test/v1/embeddings", "provider.test"),
            ("https://provider.test:443/v1/embeddings", "provider.test"),
            (
                "https://provider.test:8443/v1/embeddings",
                "provider.test:8443",
            ),
            ("http://[::1]:8080/v1/embeddings", "[::1]:8080"),
        ] {
            let target = index_authorization(&IndexOptions {
                root: Some(root.clone()),
                embedding: Some(EmbeddingModelSpec {
                    reference: "qwen/text-embedding-v4".into(),
                    revision: None,
                    cache_dir: None,
                    endpoint: Some(endpoint.into()),
                    device: Device::Auto,
                }),
                ..IndexOptions::default()
            })
            .expect("disclosure")
            .expect("consent required");
            assert_eq!(target.endpoint_host, expected);
            grant_index(&target).expect("grant");
            let status = status_snapshot(&root).expect("verified status");
            assert_eq!(status.grants.len(), 1);
            assert_eq!(status.grants[0].endpoint_host, target.endpoint_host);
            assert_eq!(revoke_all(&root).expect("revoke"), Some(1));
        }
    }
}
