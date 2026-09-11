//! Signed workspace consent shared by direct, daemon, and MCP operations.

use crate::{
    EngineError,
    models::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry},
    workspace::{
        layout::{find_nearest_workspace, workspace_index_location},
        manifest::read_workspace_manifest,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// Resolved destination and source roots disclosed before an index operation.
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
    if options.allow_remote {
        return Ok(None);
    }
    let location = workspace_index_location(&crate::workspace::layout::resolve_workspace_root(
        options.root.as_deref(),
    )?)?;
    let existing = read_workspace_manifest(&location.home)?;
    let model = crate::indexing::service::embedding_reference(
        existing.as_ref(),
        options.embedding.as_ref(),
    )?;
    if model.starts_with("local/") {
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
                    .as_ref()
                    .and_then(|m| m.embedding_runtime.endpoint.as_deref())
            }),
    )?;
    let root = fs::canonicalize(&location.root).map_err(io)?;
    if read_grant(&root)?.is_some_and(|g| g.model == model && g.endpoint == endpoint) {
        return Ok(None);
    }
    let url = reqwest::Url::parse(&endpoint)
        .map_err(|_| EngineError::invalid_argument("Invalid embedding endpoint"))?;
    let endpoint_host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    };
    // Resolve source roots through the same path selection used by indexing.
    let workspace_roots =
        crate::indexing::service::resolve_root_paths(&root, existing.as_ref(), options)
            .into_iter()
            .map(|source| source.path)
            .collect();
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
    use crate::api::context::options::{ContextRouteMode, RefreshPolicy};
    if options.rg || options.allow_remote {
        return Ok(None);
    }
    let request = crate::search::context::normalize_context_request(options)?;
    let query_text = request
        .routes
        .iter()
        .any(|route| route.mode == ContextRouteMode::Vector);
    let workspace_content = options
        .refresh
        .map_or(options.auto_update, |refresh| refresh != RefreshPolicy::Off);
    if !query_text && !workspace_content {
        return Ok(None);
    }
    let root = crate::workspace::layout::resolve_workspace_root(options.root.as_deref())?;
    let Some(location) = find_nearest_workspace(&root)? else {
        return Ok(None);
    };
    let Some(manifest) = read_workspace_manifest(&location.home)? else {
        return Ok(None);
    };
    if manifest.embedding.is_none() {
        return Ok(None);
    }
    let target = index_authorization(&crate::api::index::IndexOptions {
        root: Some(location.root),
        endpoint: options.endpoint.clone(),
        ..crate::api::index::IndexOptions::default()
    })?;
    Ok(target.map(|target| QueryAuthorization {
        target,
        query_text,
        workspace_content,
    }))
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
        fs::create_dir_all(parent).map_err(io)?;
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
    let Some(EmbeddingCatalogEntry::Qwen {
        default_endpoint, ..
    }) = get_embedding_model_catalog_entry(reference)
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
        .unwrap_or_else(|| default_endpoint.to_string());
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
                .and_then(|m| m.embedding.as_ref())
                .map(|e| format!("{}/{}", e.provider, e.model))
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
                .and_then(|m| m.embedding_runtime.endpoint.as_deref())
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
    let signature = hmac_sha256::HMAC::mac(
        serde_json::to_vec(&grant).map_err(json)?,
        signing_key(true)?,
    )
    .to_vec();
    fs::create_dir_all(&location.home).map_err(io)?;
    let temporary = location
        .home
        .join(format!(".authorization-{}", uuid::Uuid::new_v4()));
    private_write(
        &temporary,
        &serde_json::to_vec_pretty(&SignedGrant { grant, signature }).map_err(json)?,
    )?;
    let result = fs::rename(&temporary, location.home.join("authorization.json")).map_err(io);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(())
}

fn read_grant(root: &Path) -> Result<Option<Grant>, EngineError> {
    let path = root.join(".zvec-grep/authorization.json");
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(e)),
    };
    let signed: SignedGrant = serde_json::from_slice(&bytes).map_err(json)?;
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
    Ok(Some(signed.grant))
}

/// Reports verified consent without creating any state.
///
/// # Errors
/// Returns an error if existing consent cannot be verified.
pub fn status(root: &Path) -> Result<String, EngineError> {
    let root = root_path(root)?;
    let Some(grant) = read_grant(&root)? else {
        return Ok(format!(
            "Remote Embedding: not authorized\nRoot: {}",
            root.display()
        ));
    };
    Ok(format!(
        "Remote Embedding: authorized\nRoot: {}\nScope: workspace\nModel: {}\nEndpoint: {}",
        root.display(),
        grant.model,
        grant.endpoint
    ))
}

/// Removes workspace consent. Repeated revocations are harmless.
///
/// # Errors
/// Returns an error when the authorization file cannot be removed.
pub fn revoke(root: &Path) -> Result<String, EngineError> {
    let root = root_path(root)?;
    match fs::remove_file(root.join(".zvec-grep/authorization.json")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    Ok(format!(
        "Remote Embedding: not authorized\nRoot: {}",
        root.display()
    ))
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
    if read_grant(&root)?.is_some_and(|g| g.model == model && g.endpoint == endpoint) {
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
    use crate::api::index::{
        IndexOptions,
        options::{Device, DiscoveryOptions, EmbeddingModelSpec, RootPath},
    };

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
    fn index_disclosure_matches_destination_without_creating_state() {
        let directory = tempfile::tempdir().expect("workspace");
        let mut options = IndexOptions {
            root: Some(directory.path().into()),
            roots: vec![RootPath {
                path: "docs".into(),
                recursive: true,
                discovery: DiscoveryOptions::default(),
            }],
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
        assert_eq!(target.workspace_roots, vec![target.root.join("docs")]);
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
