//! Shared global model configuration for CLI and resident execution.

use crate::{
    EngineError,
    api::index::options::Device,
    utils::{atomic_write, create_directories},
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
    create_directories(parent)?;
    let _lock = crate::workspace::lock::acquire_read_write_lock(
        &parent.join("locks/config"),
        crate::workspace::lock::LockMode::Write,
        "global-config.update",
    )?;
    let mut value = read_at(path)?;
    merge(&mut value, changes);
    let bytes = serde_json::to_vec_pretty(&value)
        .map_err(|error| EngineError::internal(error.to_string()))?;
    atomic_write(path, &bytes)
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
