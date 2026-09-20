//! Shared global model configuration for CLI and resident execution.

use crate::{
    EngineError,
    domain::model::Device,
    utils::{atomic_write, sync_directory},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Returns the per-user configuration path, independently of the daemon home.
/// # Errors
/// Returns an error when the user home cannot be resolved.
pub fn global_config_path() -> Result<PathBuf, EngineError> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".zvec-grep/config.json"))
        .ok_or_else(|| EngineError::invalid_argument("Cannot determine user home directory"))
}

pub(crate) fn read() -> Result<Value, EngineError> {
    read_at(&global_config_path()?)
}

fn read_at(path: &Path) -> Result<Value, EngineError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"version": 1}));
        }
        Err(error) => return Err(EngineError::internal(error.to_string())),
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| EngineError::invalid_argument("Invalid global config JSON"))?;
    if !value.is_object() || value["version"] != 1 {
        return Err(EngineError::invalid_argument(
            "Unsupported global config version",
        ));
    }
    for key in ["defaults", "providers", "models", "client", "server"] {
        if value.get(key).is_some_and(|value| !value.is_object()) {
            return Err(EngineError::invalid_argument(format!(
                "Invalid global config field: {key}"
            )));
        }
    }
    if let Some(defaults) = value.get("defaults").and_then(Value::as_object) {
        for (key, value) in defaults {
            if !matches!(key.as_str(), "embedding" | "modelCacheDir")
                || (!value.is_string() && !value.is_null())
            {
                return Err(EngineError::invalid_argument(format!(
                    "Invalid global config field: defaults.{key}"
                )));
            }
        }
    }
    Ok(value)
}

pub(crate) fn string(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str().map(str::to_owned)
}

pub(crate) fn device(value: &Value, reference: &str) -> Option<Device> {
    serde_json::from_value(value["models"][reference]["device"].clone()).ok()
}

pub(crate) fn runtime_device(
    config: &Value,
    reference: &str,
    explicit: Option<Device>,
    workspace: Option<Device>,
) -> Result<Option<Device>, EngineError> {
    if let Some(device) = explicit.or(workspace).or_else(|| device(config, reference)) {
        return Ok(Some(device));
    }
    let Some(value) = std::env::var("ZVEC_GREP_DEVICE")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    serde_json::from_value(json!(value.trim().to_lowercase()))
        .map(Some)
        .map_err(|_| EngineError::invalid_argument("Invalid ZVEC_GREP_DEVICE"))
}

pub(crate) fn model_cache(
    config: &Value,
    explicit: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Option<PathBuf> {
    explicit
        .or(workspace)
        .or_else(|| std::env::var_os("ZVEC_GREP_MODEL_CACHE").map(PathBuf::from))
        .or_else(|| string(config, &["defaults", "modelCacheDir"]).map(PathBuf::from))
}

/// Saves provider credentials without displaying their value.
/// # Errors
/// Returns validation and configuration I/O errors.
pub fn set_provider(reference: &str, api_key: &str) -> Result<PathBuf, EngineError> {
    if reference != "qwen" {
        return Err(EngineError::invalid_argument(
            "Unsupported remote embedding provider",
        ));
    }
    update(json!({"providers": {reference: {"apiKey": api_key}}}))
}

/// Saves validated model defaults shared with the TypeScript implementation.
/// # Errors
/// Returns validation and configuration I/O errors.
pub fn set_model(
    reference: &str,
    endpoint: Option<&str>,
    device: Option<Device>,
    default_model: bool,
) -> Result<PathBuf, EngineError> {
    if crate::models::get_embedding_model_catalog_entry(reference).is_none() {
        return Err(EngineError::invalid_argument(
            "Unsupported embedding model; run zg help models",
        ));
    }
    if endpoint.is_none() && device.is_none() && !default_model {
        return Err(EngineError::invalid_argument(
            "zg config model set requires --endpoint, --device, or --default",
        ));
    }
    let local = reference.starts_with("local/");
    if local && endpoint.is_some() {
        return Err(EngineError::invalid_argument(
            "--endpoint is only supported for remote embedding models",
        ));
    }
    if !local && device.is_some() {
        return Err(EngineError::invalid_argument(
            "--device is only supported for local embedding models",
        ));
    }
    let mut patch = json!({});
    if let Some(endpoint) = endpoint {
        crate::authorization::remote_endpoint(reference, Some(endpoint))?;
        patch["models"][reference]["endpoint"] = json!(endpoint);
    }
    if let Some(device) = device {
        patch["models"][reference]["device"] = json!(device);
    }
    if default_model {
        patch["defaults"]["embedding"] = json!(reference);
    }
    update(patch)
}

fn update(changes: Value) -> Result<PathBuf, EngineError> {
    let path = global_config_path()?;
    update_at(&path, changes)?;
    Ok(path)
}

fn update_at(path: &Path, changes: Value) -> Result<(), EngineError> {
    let parent = path
        .parent()
        .ok_or_else(|| EngineError::invalid_argument("Invalid config path"))?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
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
                "create config directory '{}' (parent must already exist)",
                parent.display()
            ),
            &error,
        ));
    }
    let _lock = crate::workspace::lock::acquire_read_write_lock(
        &parent.join("locks/config"),
        crate::workspace::lock::LockMode::Write,
        "global-config.update",
    )?;
    let mut value = read_at(path)?;
    merge(&mut value, changes);
    let bytes = serde_json::to_vec_pretty(&value)
        .map_err(|error| EngineError::internal(error.to_string()))?;
    atomic_write(path, &bytes)?;
    // Retry this sync even when a previous attempt left the directory in place.
    if parent.file_name().is_some() {
        sync_directory(parent.parent().unwrap_or(parent))?;
    }
    Ok(())
}

fn merge(target: &mut Value, patch: Value) {
    if let Value::Object(fields) = patch {
        if !target.is_object() {
            *target = json!({});
        }
        for (key, value) in fields {
            merge(&mut target[&key], value);
        }
    } else {
        *target = patch;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_accept_only_current_single_model_settings() {
        let directory = tempfile::tempdir().expect("config directory");
        let path = directory.path().join("config.json");
        for defaults in [
            json!({"unknownDefault": "value"}),
            json!({"embeddingRoutes": {"text": "one", "image": "two"}}),
            json!({"embedding": ["one", "two"]}),
        ] {
            fs::write(
                &path,
                serde_json::to_vec(&json!({"version":1,"defaults":defaults})).expect("json"),
            )
            .expect("configuration");
            assert!(read_at(&path).is_err());
        }
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "version":1,"defaults":{"embedding":"local/test", "modelCacheDir":"cache"}
            }))
            .expect("json"),
        )
        .expect("configuration");
        read_at(&path).expect("single model defaults");
    }

    #[test]
    fn config_directory_requires_an_existing_outer_directory() {
        let root = tempfile::tempdir().expect("temporary directory");
        let outer = root.path().join("missing");
        let directory = outer.join("settings");
        let path = directory.join("config.json");
        let error = update_at(&path, json!({"client":{"mode":"direct"}}))
            .expect_err("outer directory must already exist");
        assert_eq!(error.code(), EngineError::NOT_FOUND);
        assert!(error.message().contains("config directory"));
        assert!(!outer.exists());

        fs::create_dir(&outer).expect("user prepares outer directory");
        fs::write(&directory, b"existing file").expect("conflicting file");
        assert!(update_at(&path, json!({})).is_err());
        assert_eq!(
            fs::read(&directory).expect("unchanged file"),
            b"existing file"
        );

        fs::remove_file(&directory).expect("remove conflict");
        update_at(&path, json!({"client":{"mode":"direct"}})).expect("retry config update");
        assert_eq!(read_at(&path).expect("config")["client"]["mode"], "direct");
    }

    #[test]
    fn updates_preserve_other_settings_and_do_not_erase_model_fields() {
        let root = tempfile::tempdir().expect("temporary directory");
        let path = root.path().join("config.json");
        update_at(
            &path,
            json!({"client":{"mode":"direct"},"models":{"local/test":{"device":"cpu"}}}),
        )
        .expect("first update");
        update_at(&path, json!({"defaults":{"embedding":"local/test"}})).expect("second update");
        let value = read_at(&path).expect("read");
        assert_eq!(value["client"]["mode"], "direct");
        assert_eq!(value["models"]["local/test"]["device"], "cpu");
        assert_eq!(value["defaults"]["embedding"], "local/test");
    }

    #[cfg(unix)]
    #[test]
    fn config_permissions_are_private_on_creation_and_preserved_on_update() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("temporary directory");
        let directory = root.path().join("settings");
        let path = directory.join("config.json");
        update_at(&path, json!({"client":{"mode":"direct"}})).expect("initial config");
        assert_eq!(
            fs::metadata(&directory)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o077,
            0
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("config metadata")
                .permissions()
                .mode()
                & 0o077,
            0
        );

        fs::set_permissions(&directory, fs::Permissions::from_mode(0o750))
            .expect("custom directory permissions");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
            .expect("custom config permissions");
        update_at(&path, json!({"defaults":{"embedding":"local/test"}}))
            .expect("update existing config");

        assert_eq!(
            fs::metadata(&directory)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("config metadata")
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        let value = read_at(&path).expect("updated config");
        assert_eq!(value["client"]["mode"], "direct");
        assert_eq!(value["defaults"]["embedding"], "local/test");
    }
}
