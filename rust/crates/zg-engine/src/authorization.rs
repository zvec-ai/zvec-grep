//! Workspace-local consent shared by direct, daemon, and MCP operations.
//!
//! Consent preflight and grant-file management are the explicit exception to
//! the `ZvecGrep` operation boundary described in `rust/CONTRIBUTING.md`.
//! They operate before engine work and never load models or send remote data.

use crate::{
    EngineError,
    domain::{Workspace, WorkspaceLayout},
    models::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry},
    utils::{
        atomic_write, current_username, ensure_directory, file_exists, reject_symlink,
        resolve_directory, sync_directory,
    },
    workspace::{
        find_nearest_workspace,
        manifest::{inspect_workspace_manifest, read_workspace_manifest},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
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

/// Workspace consent matching the current environment, without terminal presentation details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizationStatus {
    pub root: PathBuf,
    pub path: PathBuf,
    pub grants: Vec<AuthorizationGrantStatus>,
}

/// A remote destination approved for a workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizationGrantStatus {
    pub model: String,
    pub endpoint: String,
    pub endpoint_host: String,
}

/// Resolves required consent without loading models, writing state, or sending data.
///
/// # Errors
/// Returns errors for invalid workspace state, models, endpoints, or authorization records.
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
    let requested_root = resolve_directory(options.root.as_deref())?;
    let layout = WorkspaceLayout::new(requested_root.join(".zvec-grep"))?;
    let existing =
        inspect_workspace_manifest(&requested_root, &layout)?.into_workspace(options.rebuild)?;
    authorizations_for_manifest(options, &requested_root, existing.as_ref())
}

fn authorizations_for_manifest(
    options: &crate::api::index::IndexOptions,
    root: &Path,
    existing: Option<&Workspace>,
) -> Result<Vec<IndexAuthorization>, EngineError> {
    Ok(authorization_for_manifest(options, root, existing)?
        .into_iter()
        .collect())
}

fn authorization_for_manifest(
    options: &crate::api::index::IndexOptions,
    workspace_root: &Path,
    existing: Option<&Workspace>,
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
                    .and_then(|m| m.index().descriptor())
                    .and_then(|index| index.embedding_configs().get(&model))
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
    pub workspace_content: bool,
}

/// Resolves query consent without embedding text or updating the index.
/// # Errors
/// Returns invalid request, workspace, endpoint, or authorization record errors.
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
    let root = resolve_directory(options.root.as_deref())?;
    let Some((root, layout)) = find_nearest_workspace(&root)? else {
        return Ok(Vec::new());
    };
    let Some(manifest) = read_workspace_manifest(&root, &layout)? else {
        return Ok(Vec::new());
    };
    if manifest.index().descriptor().is_none() {
        return Ok(Vec::new());
    }
    if options.allow_remote || (!query_text && !workspace_content) {
        return Ok(Vec::new());
    }
    let mut authorizations: Vec<QueryAuthorization> = Vec::new();
    for (enabled, endpoint, sends_query) in [
        (query_text, options.endpoint.clone(), true),
        (workspace_content, None, false),
    ] {
        if !enabled {
            continue;
        }
        let targets = authorizations_for_manifest(
            &crate::api::index::IndexOptions {
                root: Some(root.clone()),
                endpoint,
                authorized_remote: options.authorized_remote.clone(),
                ..crate::api::index::IndexOptions::default()
            },
            &root,
            Some(&manifest),
        )?;
        for target in targets {
            if let Some(previous) = authorizations.iter_mut().find(|item| item.target == target) {
                previous.query_text |= sends_query;
                previous.workspace_content |= !sends_query;
            } else {
                authorizations.push(QueryAuthorization {
                    target,
                    query_text: sends_query,
                    workspace_content: !sends_query,
                });
            }
        }
    }
    Ok(authorizations)
}

/// Persists a destination explicitly approved by the terminal user.
///
/// # Errors
/// Returns errors for invalid destinations, an unknown username, or inaccessible consent.
pub fn grant_index(target: &IndexAuthorization) -> Result<(), EngineError> {
    let root = fs::canonicalize(&target.root).map_err(io)?;
    let endpoint = remote_endpoint(&target.model, Some(&target.endpoint))?;
    persist_grant(Grant {
        version: 1,
        root,
        username: authorization_username()?,
        capability: "embedding".into(),
        scope: "workspace".into(),
        model: target.model.clone(),
        endpoint,
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    version: u32,
    root: PathBuf,
    /// Username at explicit authorization; independent of manifest updates.
    username: String,
    capability: String,
    scope: String,
    model: String,
    endpoint: String,
}

fn authorization_username() -> Result<String, EngineError> {
    current_username().ok_or_else(|| {
        EngineError::invalid_argument(
            "Cannot determine the current username for persistent authorization; pass --allow-remote for this command only",
        )
    })
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
    let requested = resolve_directory(Some(root))?;
    let mut root = requested.clone();
    loop {
        let layout = WorkspaceLayout::new(root.join(".zvec-grep"))?;
        let has_grants = file_exists(layout.authorization_path())?;
        let has_lock = file_exists(layout.authorization_lock_path())?;
        if has_grants
            || has_lock
            || layout.has_manifest()?
            || layout.has_build_record()?
            || layout.has_index_data()?
        {
            return Ok(root);
        }
        if !root.pop() {
            return Ok(requested);
        }
    }
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
    // checked so the approved destination and the actual request URL agree.
    let path = url.path().trim_end_matches('/');
    if entry.kind == "text" && path.ends_with("/v1") {
        url.set_path(&format!("{path}/embeddings"));
    }
    Ok(url.to_string())
}

/// Persists explicit workspace consent without loading a model or contacting a provider.
///
/// # Errors
/// Returns an error for invalid models, roots, an unknown username, or inaccessible consent.
pub fn grant(
    root: &Path,
    model: Option<&str>,
    endpoint: Option<&str>,
) -> Result<String, EngineError> {
    let root = root_path(root)?;
    let layout = WorkspaceLayout::new(root.join(".zvec-grep"))?;
    let manifest = inspect_workspace_manifest(&root, &layout)?.into_workspace(true)?;
    let config = crate::config::read()?;
    let model = model
        .map(str::to_owned)
        .or_else(|| {
            manifest
                .as_ref()
                .and_then(|m| m.index().descriptor())
                .map(crate::domain::IndexDescriptor::default_embedding_model)
                .map(|e| e.model().reference())
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
                .and_then(|m| m.index().descriptor())
                .and_then(|index| index.embedding_configs().get(&model))
                .and_then(|runtime| runtime.endpoint.as_deref())
        }),
    )?;
    let grant = Grant {
        version: 1,
        root: root.clone(),
        username: authorization_username()?,
        capability: "embedding".into(),
        scope: "workspace".into(),
        model,
        endpoint,
    };
    persist_grant(grant)?;
    status(&root)
}

fn persist_grant(grant: Grant) -> Result<(), EngineError> {
    let layout = WorkspaceLayout::new(grant.root.join(".zvec-grep"))?;
    ensure_directory(layout.storage_dir())?;
    let _lock = crate::workspace::lock::try_acquire_file_lock(
        layout.authorization_lock_path(),
        crate::workspace::lock::LockMode::Exclusive,
        "authorization.grant",
    )?;
    let mut grants = read_grants(&grant.root)?;
    grants.retain(|previous| previous.model != grant.model || previous.endpoint != grant.endpoint);
    grants.push(grant);
    let bytes = serde_json::to_vec_pretty(&grants).map_err(json)?;
    atomic_write(layout.authorization_path(), &bytes)?;
    sync_directory(layout.storage_dir())
}

fn read_grants(root: &Path) -> Result<Vec<Grant>, EngineError> {
    let layout = WorkspaceLayout::new(root.join(".zvec-grep"))?;
    let path = layout.authorization_path();
    reject_symlink(path)?;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(e)),
    };
    let records: Vec<Grant> = serde_json::from_slice(&bytes).map_err(|error| {
        EngineError::invalid_argument(format!(
            "Invalid workspace authorization format: {error}; run `zg --auth revoke \"{}\"`, then authorize again with `zg --auth grant`",
            root.display()
        ))
    })?;
    let Some(username) = current_username() else {
        return Ok(Vec::new());
    };
    records
        .into_iter()
        .filter(|grant| grant.root == root && grant.username == username)
        .map(|grant| {
            if grant.version != 1
                || grant.capability != "embedding"
                || grant.scope != "workspace"
            {
                return Err(EngineError::permission_denied(format!(
                    "Invalid workspace authorization version, capability, or scope; run `zg --auth revoke \"{}\"`, then authorize again with `zg --auth grant`",
                    root.display()
                )));
            }
            Ok(grant)
        })
        .collect()
}

/// Reports consent matching the current environment without creating any state.
///
/// # Errors
/// Returns an error if existing consent cannot be read or validated.
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
        path: WorkspaceLayout::new(root.join(".zvec-grep"))?
            .authorization_path()
            .to_path_buf(),
        root,
        grants,
    })
}

/// Reports consent matching the current environment without creating any state.
///
/// # Errors
/// Returns an error if existing consent cannot be read or validated.
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
/// Invalid consent can still be removed without a matching environment.
/// Returns `None` when removed consent could not be read or parsed to count records.
///
/// # Errors
/// Returns an error when authorization state cannot be removed or synced.
pub fn revoke_all(root: &Path) -> Result<Option<usize>, EngineError> {
    let root = root_path(root)?;
    let layout = WorkspaceLayout::new(root.join(".zvec-grep"))?;
    let path = layout.authorization_path();
    if !file_exists(path)? {
        return Ok(Some(0));
    }
    let _lock = crate::workspace::lock::try_acquire_file_lock(
        layout.authorization_lock_path(),
        crate::workspace::lock::LockMode::Exclusive,
        "authorization.revoke",
    )?;
    let count = match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Vec<Grant>>(&bytes) {
            Ok(records) => Some(records.len()),
            // Explicit revocation remains the recovery path for malformed consent.
            Err(_) => None,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Some(0)),
        // Revocation must also work when existing consent cannot be read.
        Err(_) => None,
    };
    match fs::remove_file(path) {
        Ok(()) => sync_directory(layout.storage_dir())?,
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

    #[cfg(unix)]
    #[test]
    fn unrelated_storage_symlinks_do_not_block_authorization() {
        use std::os::unix::fs::symlink;

        for name in ["index", "temp/new"] {
            let directory = tempfile::tempdir().expect("workspace");
            let root = directory.path().join("source");
            let home = root.join(".zvec-grep");
            fs::create_dir_all(home.join("temp")).expect("workspace home");
            let external = directory.path().join("external");
            fs::create_dir(&external).expect("external directory");
            symlink(&external, home.join(name)).expect("storage alias");

            assert!(read_grants(&root).expect("read missing grants").is_empty());
            let path = home.join("authorization.json");
            fs::write(&path, b"invalid consent").expect("malformed consent");
            assert_eq!(revoke_all(&root).expect("revoke malformed consent"), None);
            assert!(!path.exists());
            assert!(
                fs::symlink_metadata(home.join(name))
                    .expect("storage alias remains")
                    .is_symlink()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn authorization_rejects_symlinked_home_and_grants() {
        use std::os::unix::fs::symlink;

        for home_alias in [true, false] {
            let directory = tempfile::tempdir().expect("workspace");
            let root = directory.path().join("source");
            fs::create_dir(&root).expect("source directory");
            let home = root.join(".zvec-grep");
            let external = directory.path().join("external");
            fs::create_dir(&external).expect("external directory");
            let external_grants = external.join("authorization.json");
            fs::write(&external_grants, b"external consent").expect("external grants");
            let alias = if home_alias {
                symlink(&external, &home).expect("home alias");
                home.clone()
            } else {
                fs::create_dir(&home).expect("workspace home");
                let path = home.join("authorization.json");
                symlink(&external_grants, &path).expect("grants alias");
                path
            };

            assert!(
                read_grants(&root)
                    .err()
                    .expect("reject reading alias")
                    .message()
                    .contains("symbolic link")
            );
            assert!(
                revoke_all(&root)
                    .expect_err("reject revoking alias")
                    .message()
                    .contains("symbolic link")
            );
            assert!(
                persist_grant(Grant {
                    version: 1,
                    root,
                    username: "authorization-test".into(),
                    capability: "embedding".into(),
                    scope: "workspace".into(),
                    model: "qwen/text-embedding-v4".into(),
                    endpoint: "https://example.test/embeddings".into(),
                })
                .expect_err("reject writing alias")
                .message()
                .contains("symbolic link")
            );
            assert_eq!(
                fs::read(&external_grants).expect("external grants"),
                b"external consent"
            );
            assert!(
                fs::symlink_metadata(alias)
                    .expect("alias remains")
                    .is_symlink()
            );
        }
    }

    fn isolated_authorization_root(test_name: &str) -> Option<PathBuf> {
        const FIXTURE_ENV: &str = "ZG_AUTHORIZATION_RECOVERY_TEST_ROOT";
        if let Some(root) = env::var_os(FIXTURE_ENV) {
            return Some(PathBuf::from(root));
        }
        // Child-process configuration keeps usernames and global settings isolated
        // without changing environment variables while other unit tests are running.
        let workspace = tempfile::tempdir().expect("workspace");
        let state = tempfile::tempdir().expect("authorization state");
        let output = std::process::Command::new(env::current_exe().expect("test executable"))
            .args(["--exact", test_name, "--nocapture"])
            .env(FIXTURE_ENV, workspace.path())
            .env("HOME", state.path())
            .env("USERPROFILE", state.path())
            .env("USER", "authorization-test")
            .env("USERNAME", "authorization-test")
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
    fn authorization_management_preserves_nested_boundaries() {
        let Some(root) = isolated_authorization_root(
            "authorization::tests::authorization_management_preserves_nested_boundaries",
        ) else {
            return;
        };
        let root = fs::canonicalize(root).expect("parent root");
        persist_grant(Grant {
            version: 1,
            root: root.clone(),
            username: authorization_username().expect("username"),
            capability: "embedding".into(),
            scope: "workspace".into(),
            model: "qwen/text-embedding-v4".into(),
            endpoint: "https://parent.test/embeddings".into(),
        })
        .expect("parent grant");
        fs::write(root.join(".zvec-grep/manifest.json"), b"{}").expect("parent workspace marker");

        for (name, orphaned_index) in [("grants-only", false), ("orphaned-index", true)] {
            let child = root.join(name);
            let nested = child.join("src/nested");
            fs::create_dir_all(&nested).expect("nested source");
            let layout = WorkspaceLayout::new(child.join(".zvec-grep")).expect("child layout");
            if orphaned_index {
                fs::create_dir_all(layout.index_dir()).expect("orphaned index");
                let status = status_snapshot(&nested).expect("status without manifest");
                assert_eq!(status.root, child);
                assert!(status.grants.is_empty());
            } else {
                fs::create_dir(layout.storage_dir()).expect("child storage");
                fs::write(layout.authorization_path(), b"[]")
                    .expect("child authorization boundary");
            }
            grant(
                &child,
                Some("qwen/text-embedding-v4"),
                Some("https://child.test/embeddings"),
            )
            .expect("child grant");
            if !orphaned_index {
                fs::remove_file(layout.authorization_lock_path())
                    .expect("copied grants lack a lock");
            }
            let status = status_snapshot(&nested).expect("child status");
            assert_eq!(status.root, child);
            assert_eq!(status.grants.len(), 1);
            assert_eq!(status.grants[0].endpoint_host, "child.test");
            assert_eq!(revoke_all(&nested).expect("revoke child"), Some(1));
            assert_eq!(revoke_all(&nested).expect("repeat revoke"), Some(0));
            let status = status_snapshot(&nested).expect("revoked child status");
            assert_eq!(status.root, child);
            assert!(status.grants.is_empty());
            assert!(layout.authorization_lock_path().is_file());
            assert_eq!(layout.index_dir().exists(), orphaned_index);
            assert_eq!(
                status_snapshot(&root).expect("parent status").grants.len(),
                1
            );
        }
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
    fn invalid_version_capability_or_scope_explains_revoke_before_regrant() {
        let Some(root) = isolated_authorization_root(
            "authorization::tests::invalid_version_capability_or_scope_explains_revoke_before_regrant",
        ) else {
            return;
        };
        let root = fs::canonicalize(root).expect("canonical root");
        let path = WorkspaceLayout::new(root.join(".zvec-grep"))
            .expect("layout")
            .authorization_path()
            .to_path_buf();
        for field in ["version", "capability", "scope"] {
            grant(
                &root,
                Some("qwen/text-embedding-v4"),
                Some("https://provider.test/embeddings"),
            )
            .expect("valid authorization");
            let mut records: Vec<Grant> =
                serde_json::from_slice(&fs::read(&path).expect("grant")).expect("grant list");
            match field {
                "version" => records[0].version = 0,
                "capability" => records[0].capability = "unknown".into(),
                _ => records[0].scope = "once".into(),
            }
            let bytes = serde_json::to_vec(&records).expect("invalid authorization fixture");
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
        request.endpoint = Some("https://temporary.test/embeddings".into());
        let plans = query_authorizations(&request).expect("separate query and refresh consent");
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].target.endpoint_host, "temporary.test");
        assert!(plans[0].query_text && !plans[0].workspace_content);
        assert_eq!(plans[1].target.endpoint_host, "query.test");
        assert!(!plans[1].query_text && plans[1].workspace_content);
        request.authorized_remote = vec![plans[0].target.clone()];
        assert_eq!(
            query_authorizations(&request).expect("query-only consent"),
            vec![plans[1].clone()]
        );
        request.authorized_remote.clear();
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
        assert_eq!(plan.target.endpoint_host, "query.test");
        assert_eq!(
            fs::read(&manifest).expect("manifest unchanged by preflight"),
            before
        );
        request.allow_remote = true;
        assert!(
            query_authorization(&request)
                .expect("explicit consent")
                .is_none()
        );
    }

    #[tokio::test]
    async fn index_preflight_does_not_read_ignore_dependencies() {
        let directory = tempfile::tempdir().expect("fixture");
        let root = directory.path().join("source");
        fs::create_dir(&root).expect("source");
        let ignored = directory.path().join("external.ignore");
        fs::write(&ignored, "*.log\n").expect("external ignore");
        let engine = crate::ZvecGrep::new();
        engine
            .index(IndexOptions {
                root: Some(root.clone()),
                embedding: Some(EmbeddingModelSpec {
                    reference: "qwen/text-embedding-v4".into(),
                    revision: None,
                    cache_dir: None,
                    endpoint: None,
                    device: Device::Auto,
                }),
                api_key: Some("test-key".into()),
                endpoint: Some("https://provider.test/embeddings".into()),
                allow_remote: true,
                scan: crate::api::index::options::ScanRulesUpdate {
                    ignore_files: Some(vec![ignored.clone()]),
                    ..Default::default()
                },
                ..IndexOptions::default()
            })
            .await
            .expect("empty index");
        engine.close();
        let manifest = root.join(".zvec-grep/manifest.json");
        let before = fs::read(&manifest).expect("manifest");
        fs::remove_file(&ignored).expect("remove external dependency");
        let mut request = IndexOptions {
            root: Some(root.clone()),
            ..IndexOptions::default()
        };
        let plans = index_authorizations(&request).expect("metadata does not read ignore files");
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].endpoint_host, "provider.test");
        request.reset_paths = true;
        assert_eq!(
            index_authorizations(&request).expect("reset preflight"),
            plans
        );
        request.reset_paths = false;
        let replacement = root.join("new.ignore");
        fs::write(&replacement, "*.tmp\n").expect("replacement");
        request.scan.ignore_files = Some(vec![replacement]);
        assert_eq!(
            index_authorizations(&request).expect("replacement can reach authorization"),
            plans
        );
        assert_eq!(fs::read(&manifest).expect("unmodified manifest"), before);
    }

    fn remote_workspace(layout: WorkspaceLayout, endpoint: &str) -> Workspace {
        use crate::domain::{
            IndexDescriptor, IndexState,
            model::{EmbeddingMetric, EmbeddingModelInfo},
        };
        let source_root = layout
            .storage_dir()
            .parent()
            .expect("source root")
            .to_path_buf();
        Workspace::new(
            &source_root,
            layout,
            crate::domain::ScanRules::default(),
            Some(crate::workspace::CURRENT_INDEX_VERSION),
            Some(uuid::Uuid::new_v4().to_string()),
            1,
            1,
            IndexState::Enabled(Box::new(
                IndexDescriptor::new(
                    EmbeddingModelInfo::new(
                        crate::domain::model::ModelInfo::new(
                            "qwen",
                            "text-embedding-v4",
                            [
                                crate::domain::ContentKind::Text,
                                crate::domain::ContentKind::Code,
                                crate::domain::ContentKind::Image,
                            ],
                            32,
                        )
                        .expect("valid model metadata"),
                        1024,
                        EmbeddingMetric::Cosine,
                        None,
                        None,
                    )
                    .expect("valid embedding metadata"),
                    std::collections::BTreeMap::new(),
                    std::collections::BTreeMap::from([(
                        "qwen/text-embedding-v4".into(),
                        crate::domain::model::ModelConfig {
                            endpoint: Some(endpoint.into()),
                            ..Default::default()
                        },
                    )]),
                    crate::domain::FTS_CONFIG,
                )
                .expect("index descriptor"),
            )),
        )
        .expect("manifest")
    }

    #[test]
    fn saving_a_moved_workspace_does_not_refresh_authorization() {
        let Some(directory) = isolated_authorization_root(
            "authorization::tests::saving_a_moved_workspace_does_not_refresh_authorization",
        ) else {
            return;
        };
        let original = directory.join("original");
        fs::create_dir(&original).expect("source directory");
        let endpoint = "https://provider.test/embeddings";
        grant(&original, Some("qwen/text-embedding-v4"), Some(endpoint)).expect("original consent");
        let moved = directory.join("moved");
        fs::rename(original, &moved).expect("move source and storage");
        let layout = WorkspaceLayout::new(moved.join(".zvec-grep")).expect("moved layout");
        let before = fs::read(layout.authorization_path()).expect("carried consent");

        // Saving current source history must not approve the new location.
        let workspace = remote_workspace(layout.clone(), endpoint);
        crate::workspace::manifest::write_workspace_manifest(layout.manifest_path(), &workspace)
            .expect("save current environment");
        let options = IndexOptions {
            root: Some(moved.clone()),
            ..IndexOptions::default()
        };
        assert_eq!(
            index_authorizations(&options)
                .expect("new consent needed")
                .len(),
            1
        );
        assert_eq!(
            fs::read(layout.authorization_path()).expect("unchanged consent"),
            before
        );

        grant(&moved, None, None).expect("explicitly approve new location");
        assert!(
            index_authorizations(&options)
                .expect("new grant applies")
                .is_empty()
        );
    }

    #[test]
    fn abandoned_build_does_not_override_active_authorization() {
        use crate::{
            api::context::{ContextOptions, options::RefreshPolicy},
            workspace::{index::prepare_build, manifest::write_workspace_manifest},
        };
        let directory = tempfile::tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let active = remote_workspace(layout.clone(), "https://active.test/embeddings");
        fs::create_dir_all(active.layout().index_dir()).expect("active generation");
        write_workspace_manifest(active.layout().manifest_path(), &active).expect("write active");
        let target = remote_workspace(layout, "https://staging.test/embeddings");
        prepare_build(&target).expect("staged build");
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
        fs::remove_file(active.layout().manifest_path()).expect("unpublished workspace");
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

    #[tokio::test]
    async fn nested_index_authorization_ignores_parent_manifest_and_grants() {
        let Some(root) = isolated_authorization_root(
            "authorization::tests::nested_index_authorization_ignores_parent_manifest_and_grants",
        ) else {
            return;
        };
        let engine = crate::ZvecGrep::new();
        let parent_options = IndexOptions {
            root: Some(root.clone()),
            embedding: Some(EmbeddingModelSpec {
                reference: "qwen/text-embedding-v4".into(),
                revision: None,
                cache_dir: None,
                endpoint: Some("https://parent.test/embeddings".into()),
                device: Device::Auto,
            }),
            api_key: Some("parent-test-key".into()),
            allow_remote: true,
            ..IndexOptions::default()
        };
        engine
            .index(parent_options.clone())
            .await
            .expect("empty parent index");
        engine.close();
        let parent_manifest = root.join(".zvec-grep/manifest.json");
        let before = fs::read(&parent_manifest).expect("parent manifest");
        let mut unapproved = parent_options;
        unapproved.allow_remote = false;
        let parent_target = index_authorization(&unapproved)
            .expect("parent plan")
            .expect("parent consent");
        grant_index(&parent_target).expect("parent grant");

        let child = root.join("sub");
        fs::create_dir(&child).expect("child directory");
        let mut options = IndexOptions {
            root: Some(child.clone()),
            ..unapproved.clone()
        };
        let target = index_authorization(&options)
            .expect("child plan")
            .expect("parent grant does not authorize child");
        assert_eq!(target.root, fs::canonicalize(&child).expect("child root"));
        assert_eq!(target.workspace_roots, vec![target.root.clone()]);
        options.embedding.as_mut().expect("child model").reference =
            "qwen/qwen3.7-text-embedding".into();
        options.endpoint = Some("https://child.test/embeddings".into());
        for rebuild in [false, true] {
            options.rebuild = rebuild;
            let target = index_authorization(&options)
                .expect("independent child plan")
                .expect("child consent");
            assert_eq!(target.root, fs::canonicalize(&child).expect("child root"));
            assert_eq!(target.model, "qwen/qwen3.7-text-embedding");
            assert_eq!(target.endpoint, "https://child.test/embeddings");
        }
        assert_eq!(
            fs::read(parent_manifest).expect("parent manifest unchanged"),
            before
        );
        assert!(
            !child.join(".zvec-grep").exists(),
            "preflight does not create child state"
        );
    }
}
