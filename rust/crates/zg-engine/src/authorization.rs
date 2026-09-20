//! Signed workspace consent shared by direct, daemon, and MCP operations.

use crate::{
    EngineError,
    models::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry},
    utils::{atomic_write, sync_directory},
    workspace::{
        layout::{find_nearest_workspace, workspace_index_location},
        manifest::{WorkspaceManifest, read_workspace_manifest},
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
    let existing = read_workspace_manifest(&location.home)?;
    authorizations_for_manifest(options, &location.root, existing.as_ref())
}

fn authorizations_for_manifest(
    options: &crate::api::index::IndexOptions,
    root: &Path,
    existing: Option<&WorkspaceManifest>,
) -> Result<Vec<IndexAuthorization>, EngineError> {
    Ok(authorization_for_manifest(options, root, existing)?
        .into_iter()
        .collect())
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
    let endpoint_host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    };
    let workspace_roots = vec![root.clone()];
    Ok(Some(IndexAuthorization {
        root,
        workspace_roots,
        model,
        endpoint,
        endpoint_host,
    }))
}

/// Destination and data categories requiring consent for an indexed query.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueryAuthorization {
    pub target: IndexAuthorization,
    pub query_text: bool,
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
    let query_text = request
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
    if manifest.workspace.index.descriptor().is_none() {
        return Ok(Vec::new());
    }
    if options.allow_remote || (!query_text && !workspace_content) {
        return Ok(Vec::new());
    }
    let targets = authorizations_for_manifest(
        &crate::api::index::IndexOptions {
            root: Some(location.root.clone()),
            endpoint: options.endpoint.clone(),
            authorized_remote: options.authorized_remote.clone(),
            ..crate::api::index::IndexOptions::default()
        },
        &location.root,
        Some(&manifest),
    )?;
    Ok(targets
        .into_iter()
        .map(|target| QueryAuthorization {
            target,
            query_text,
            workspace_content,
        })
        .collect())
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
    let url = reqwest::Url::parse(endpoint.trim())
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
    let manifest = read_workspace_manifest(&location.home)?;
    let config = crate::config::read()?;
    let model = model
        .map(str::to_owned)
        .or_else(|| {
            manifest
                .as_ref()
                .and_then(|m| m.embedding())
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
    let records: SignedGrants = serde_json::from_slice(&bytes).map_err(json)?;
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
                return Err(EngineError::permission_denied(
                    "Invalid workspace authorization signature or scope; run zg auth grant again",
                ));
            }
            Ok(signed.grant)
        })
        .collect()
}

/// Reports verified consent without creating any state.
///
/// # Errors
/// Returns an error if existing consent cannot be verified.
pub fn status(root: &Path) -> Result<String, EngineError> {
    let root = root_path(root)?;
    let grants = read_grants(&root)?;
    if grants.is_empty() {
        return Ok(format!(
            "Remote Embedding: not authorized\nRoot: {}",
            root.display()
        ));
    }
    let destinations = grants
        .iter()
        .map(|grant| format!("Model: {}\nEndpoint: {}", grant.model, grant.endpoint))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!(
        "Remote Embedding: authorized\nRoot: {}\nScope: workspace\n{destinations}",
        root.display()
    ))
}

/// Removes workspace consent. Repeated revocations are harmless.
///
/// # Errors
/// Returns an error when the authorization file cannot be removed or its deletion synced.
pub fn revoke(root: &Path) -> Result<String, EngineError> {
    let root = root_path(root)?;
    let home = root.join(".zvec-grep");
    match fs::remove_file(home.join("authorization.json")) {
        Ok(()) => sync_directory(&home)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    Ok(format!(
        "Remote Embedding: not authorized\nRoot: {}",
        root.display()
    ))
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
        "Remote embedding authorization required for {model} at {endpoint}. Run zg auth grant \"{}\" --capability embedding --scope workspace --embedding {model} --endpoint \"{endpoint}\", or pass --allow-remote for this command only.",
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
                model::{EmbeddingModelInfo, Metric, ModelConfig},
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
                    model: crate::domain::model::ModelInfo {
                        provider: "qwen".into(),
                        name: "text-embedding-v4".into(),
                        endpoint: None,
                    },
                    dimension: 1024,
                    metric: Metric::Cosine,
                    max_batch_size: 32,
                    max_input_tokens: None,
                    max_image_bytes: None,
                })),
                created_epoch_ms: 1,
                updated_epoch_ms: 1,
            },
            home.clone(),
            Some(5),
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
        prepare_build(target, Some(&active)).expect("staged build");
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
}
