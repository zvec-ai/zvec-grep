//! Shared per-user global configuration for CLI and resident execution.

use crate::{
    EngineError,
    domain::model::Device,
    utils::{atomic_write, sync_directory},
};
use serde_json::{Map, Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Returns the per-user configuration path, independently of the daemon home.
/// # Errors
/// Returns an error when the user home cannot be resolved.
pub fn global_config_path() -> Result<PathBuf, EngineError> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    home.map(|home| PathBuf::from(home).join(".zvec-grep/config.json"))
        .ok_or_else(|| EngineError::invalid_argument("Cannot determine user home directory"))
}

pub(crate) fn read() -> Result<Value, EngineError> {
    read_at(&global_config_path()?)
}

/// Reads the configured client mode after validating the global configuration.
/// # Errors
/// Returns configuration I/O and validation errors.
pub fn client_mode() -> Result<Option<String>, EngineError> {
    Ok(string(&read()?, &["client", "mode"]))
}

/// Resolves the configured loopback listen address with server defaults.
/// # Errors
/// Returns configuration I/O, validation, or non-loopback host errors.
pub fn server_listen() -> Result<String, EngineError> {
    listen_from_config(&read()?)
}

/// Resolves the configured MCP URL, preferring the explicit client URL.
/// # Errors
/// Returns configuration I/O, validation, or non-loopback host errors.
pub fn configured_server_url() -> Result<String, EngineError> {
    server_url_from_config(&read()?)
}

fn server_url_from_config(config: &Value) -> Result<String, EngineError> {
    if let Some(url) = string(config, &["client", "serverUrl"]) {
        return Ok(url);
    }
    Ok(format!("http://{}/mcp", listen_from_config(config)?))
}

fn listen_from_config(config: &Value) -> Result<String, EngineError> {
    let host = string(config, &["server", "host"]).unwrap_or_else(|| "127.0.0.1".to_owned());
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(&host);
    let normalized = unbracketed.to_ascii_lowercase();
    if !matches!(normalized.as_str(), "127.0.0.1" | "::1" | "localhost") {
        return Err(EngineError::invalid_argument(
            "Server listen host must be loopback",
        ));
    }
    let host = if normalized == "localhost" {
        &normalized
    } else {
        unbracketed
    };
    let port = config["server"]["port"].as_u64().unwrap_or(7999);
    if host.contains(':') {
        Ok(format!("[{host}]:{port}"))
    } else {
        Ok(format!("{host}:{port}"))
    }
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
    parse_global_config(&value)
}

fn parse_global_config(value: &Value) -> Result<Value, EngineError> {
    let Some(fields) = value.as_object() else {
        return Err(EngineError::invalid_argument(
            "Unsupported global config version",
        ));
    };
    if fields.get("version").and_then(Value::as_f64) != Some(1.0) {
        return Err(EngineError::invalid_argument(
            "Unsupported global config version",
        ));
    }
    check_known_fields(
        fields,
        &[
            "version",
            "defaults",
            "providers",
            "models",
            "client",
            "server",
            "log",
        ],
        "config",
    )?;

    let mut parsed = Map::new();
    parsed.insert("version".to_owned(), json!(1));
    for (name, parser) in [
        (
            "defaults",
            parse_defaults as fn(&Map<String, Value>) -> Result<Option<Value>, EngineError>,
        ),
        ("providers", parse_providers),
        ("models", parse_models),
        ("client", parse_client),
        ("server", parse_server),
        ("log", parse_log),
    ] {
        if let Some(section) = fields.get(name) {
            let section = section.as_object().ok_or_else(|| invalid_field(name))?;
            if let Some(section) = parser(section)? {
                parsed.insert(name.to_owned(), section);
            }
        }
    }
    Ok(Value::Object(parsed))
}

fn parse_defaults(fields: &Map<String, Value>) -> Result<Option<Value>, EngineError> {
    check_known_fields(fields, &["embedding", "modelCacheDir"], "defaults")?;
    let mut parsed = Map::new();
    for name in ["embedding", "modelCacheDir"] {
        if let Some(value) = non_empty_string(fields, name, &format!("defaults.{name}"))? {
            parsed.insert(name.to_owned(), json!(value));
        }
    }
    Ok(non_empty_object(parsed))
}

fn parse_providers(fields: &Map<String, Value>) -> Result<Option<Value>, EngineError> {
    let mut parsed = Map::new();
    for (provider, value) in fields {
        let path = format!("providers.{provider}");
        if !valid_provider_name(provider) {
            return Err(invalid_field(&path));
        }
        let provider_fields = value.as_object().ok_or_else(|| invalid_field(&path))?;
        check_known_fields(provider_fields, &["apiKey"], &path)?;
        if let Some(api_key) =
            non_empty_string(provider_fields, "apiKey", &format!("{path}.apiKey"))?
        {
            parsed.insert(provider.to_owned(), json!({ "apiKey": api_key }));
        }
    }
    Ok(non_empty_object(parsed))
}

fn parse_models(fields: &Map<String, Value>) -> Result<Option<Value>, EngineError> {
    let mut parsed = Map::new();
    for (reference, value) in fields {
        let path = format!("models.{reference}");
        if !valid_model_reference(reference) {
            return Err(invalid_field(&path));
        }
        let model_fields = value.as_object().ok_or_else(|| invalid_field(&path))?;
        check_known_fields(model_fields, &["endpoint", "device"], &path)?;
        let endpoint = non_empty_string(model_fields, "endpoint", &format!("{path}.endpoint"))?;
        if let Some(endpoint) = &endpoint {
            let valid = reqwest::Url::parse(endpoint)
                .is_ok_and(|url| matches!(url.scheme(), "http" | "https"));
            if !valid || reference.starts_with("local/") {
                return Err(invalid_field(&format!("{path}.endpoint")));
            }
        }
        let device = model_fields.get("device");
        if device.is_some_and(|device| {
            !matches!(
                device.as_str(),
                Some("auto" | "cpu" | "metal" | "vulkan" | "cuda")
            ) || !reference.starts_with("local/")
        }) {
            return Err(invalid_field(&format!("{path}.device")));
        }
        let mut model = Map::new();
        if let Some(endpoint) = endpoint {
            model.insert("endpoint".to_owned(), json!(endpoint));
        }
        if let Some(device) = device {
            model.insert("device".to_owned(), device.clone());
        }
        if !model.is_empty() {
            parsed.insert(reference.to_owned(), Value::Object(model));
        }
    }
    Ok(non_empty_object(parsed))
}

fn parse_client(fields: &Map<String, Value>) -> Result<Option<Value>, EngineError> {
    check_known_fields(fields, &["mode", "serverUrl"], "client")?;
    let mut parsed = Map::new();
    if let Some(mode) = fields.get("mode") {
        if !matches!(mode.as_str(), Some("direct" | "server" | "auto")) {
            return Err(invalid_field("client.mode"));
        }
        parsed.insert("mode".to_owned(), mode.clone());
    }
    if let Some(server_url) = non_empty_string(fields, "serverUrl", "client.serverUrl")? {
        parsed.insert("serverUrl".to_owned(), json!(server_url));
    }
    Ok(non_empty_object(parsed))
}

fn parse_server(fields: &Map<String, Value>) -> Result<Option<Value>, EngineError> {
    check_known_fields(fields, &["host", "port"], "server")?;
    let mut parsed = Map::new();
    if let Some(host) = non_empty_string(fields, "host", "server.host")? {
        parsed.insert("host".to_owned(), json!(host));
    }
    if let Some(port) = fields.get("port") {
        let port =
            json_integer_in_range(port, 1, 65_535).ok_or_else(|| invalid_field("server.port"))?;
        parsed.insert("port".to_owned(), json!(port));
    }
    Ok(non_empty_object(parsed))
}

fn parse_log(fields: &Map<String, Value>) -> Result<Option<Value>, EngineError> {
    check_known_fields(fields, &["maxBytes", "keep", "level"], "log")?;
    let mut parsed = Map::new();
    for (name, minimum) in [("maxBytes", 1_u64), ("keep", 0)] {
        if let Some(value) = fields.get(name) {
            let number = json_integer_in_range(value, minimum, 9_007_199_254_740_991)
                .ok_or_else(|| invalid_field(&format!("log.{name}")))?;
            parsed.insert(name.to_owned(), json!(number));
        }
    }
    if let Some(level) = fields.get("level") {
        if !matches!(level.as_str(), Some("info" | "debug")) {
            return Err(invalid_field("log.level"));
        }
        parsed.insert("level".to_owned(), level.clone());
    }
    Ok(non_empty_object(parsed))
}

fn json_integer_in_range(value: &Value, minimum: u64, maximum: u64) -> Option<u64> {
    let number = if let Some(number) = value.as_u64() {
        number
    } else {
        let float = value.as_f64()?;
        if !float.is_finite() || float.fract() != 0.0 {
            return None;
        }
        if float == 0.0 {
            0
        } else {
            format!("{float:.0}").parse().ok()?
        }
    };
    (minimum..=maximum).contains(&number).then_some(number)
}

fn check_known_fields(
    fields: &Map<String, Value>,
    allowed: &[&str],
    path: &str,
) -> Result<(), EngineError> {
    for name in fields.keys() {
        if !allowed.contains(&name.as_str()) {
            return Err(invalid_field(&format!("{path}.{name}")));
        }
    }
    Ok(())
}

fn non_empty_string(
    fields: &Map<String, Value>,
    name: &str,
    path: &str,
) -> Result<Option<String>, EngineError> {
    let Some(value) = fields.get(name) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| invalid_field(path))
}

fn valid_provider_name(value: &str) -> bool {
    let mut characters = value.bytes();
    characters
        .next()
        .is_some_and(|value| value.is_ascii_lowercase())
        && characters.all(|value| {
            value.is_ascii_lowercase() || value.is_ascii_digit() || matches!(value, b'_' | b'-')
        })
}

fn valid_model_reference(value: &str) -> bool {
    let Some((provider, model)) = value.split_once('/') else {
        return false;
    };
    valid_provider_name(provider)
        && !model.is_empty()
        && model
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'.' | b'_' | b'-'))
}

fn non_empty_object(fields: Map<String, Value>) -> Option<Value> {
    (!fields.is_empty()).then_some(Value::Object(fields))
}

fn invalid_field(path: &str) -> EngineError {
    EngineError::invalid_argument(format!("Invalid global config field: {path}"))
}

/// Size-based daemon log limits shared with the TypeScript server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DaemonLogOptions {
    pub max_bytes: u64,
    pub keep: u64,
    pub debug: bool,
}

/// Reads the daemon logging settings from the per-user global configuration.
/// # Errors
/// Returns configuration I/O and validation errors.
pub fn daemon_log_options() -> Result<DaemonLogOptions, EngineError> {
    let config = read()?;
    let log = &config["log"];
    Ok(DaemonLogOptions {
        max_bytes: log["maxBytes"].as_u64().unwrap_or(10 * 1024 * 1024),
        keep: log["keep"].as_u64().unwrap_or(5),
        debug: log["level"] == "debug",
    })
}

pub(crate) fn string(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str().map(str::to_owned)
}

/// Selects a local model for CLI-only implicit indexing, never a remote default.
/// # Errors
/// Returns configuration I/O, validation, and invalid environment model errors.
pub fn implicit_embedding_reference() -> Result<String, EngineError> {
    let config = read()?;
    let configured = crate::models::resolve_embedding_reference(
        crate::models::ResolveEmbeddingReferenceOptions {
            global_default: string(&config, &["defaults", "embedding"]),
            ..crate::models::ResolveEmbeddingReferenceOptions::default()
        },
    )
    .map_err(crate::models::ModelError::into_engine_error)?;
    Ok(configured
        .filter(|reference| reference.starts_with("local/"))
        .unwrap_or_else(|| "local/potion-code-16m-v2".to_owned()))
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
            "Unsupported embedding model; run zg --help models",
        ));
    }
    if endpoint.is_none() && device.is_none() && !default_model {
        return Err(EngineError::invalid_argument(
            "zg --config model set requires --endpoint, --device, or --default",
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
    let value = parse_global_config(&value)?;
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
    fn global_config_rejects_invalid_nested_fields() {
        let invalid = [
            (json!({"version": 1, "unknown": true}), "config.unknown"),
            (
                json!({"version": 1, "defaults": {"embedding": null}}),
                "defaults.embedding",
            ),
            (
                json!({"version": 1, "defaults": {"modelCacheDir": "  "}}),
                "defaults.modelCacheDir",
            ),
            (json!({"version": 1, "providers": []}), "providers"),
            (
                json!({"version": 1, "providers": {"Qwen": {}}}),
                "providers.Qwen",
            ),
            (
                json!({"version": 1, "providers": {"qwen": null}}),
                "providers.qwen",
            ),
            (
                json!({"version": 1, "providers": {"qwen": {"apiKey": "  "}}}),
                "providers.qwen.apiKey",
            ),
            (
                json!({"version": 1, "providers": {"qwen": {"apiKey": 12345}}}),
                "providers.qwen.apiKey",
            ),
            (
                json!({"version": 1, "providers": {"qwen": {"secret": true}}}),
                "providers.qwen.secret",
            ),
        ];
        assert_invalid_configs(&invalid);
    }

    #[test]
    fn global_config_rejects_invalid_models() {
        let invalid = [
            (json!({"version": 1, "models": []}), "models"),
            (
                json!({"version": 1, "models": {"bad/reference/extra": {}}}),
                "models.bad/reference/extra",
            ),
            (
                json!({"version": 1, "models": {"qwen/model": null}}),
                "models.qwen/model",
            ),
            (
                json!({"version": 1, "models": {"qwen/model": {"unknown": true}}}),
                "models.qwen/model.unknown",
            ),
            (
                json!({"version": 1, "models": {"qwen/model": {"endpoint": "  "}}}),
                "models.qwen/model.endpoint",
            ),
            (
                json!({"version": 1, "models": {"qwen/model": {"endpoint": "ftp://example.test"}}}),
                "models.qwen/model.endpoint",
            ),
            (
                json!({"version": 1, "models": {"qwen/model": {"device": "cpu"}}}),
                "models.qwen/model.device",
            ),
            (
                json!({"version": 1, "models": {"local/model": {"endpoint": "https://example.test"}}}),
                "models.local/model.endpoint",
            ),
            (
                json!({"version": 1, "models": {"local/model": {"device": "gpu"}}}),
                "models.local/model.device",
            ),
        ];
        assert_invalid_configs(&invalid);
    }

    #[test]
    fn global_config_rejects_invalid_client_fields() {
        let invalid = [
            (json!({"version": 1, "client": []}), "client"),
            (
                json!({"version": 1, "client": {"mode": "invalid"}}),
                "client.mode",
            ),
            (
                json!({"version": 1, "client": {"serverUrl": null}}),
                "client.serverUrl",
            ),
            (
                json!({"version": 1, "client": {"serverUrl": "  "}}),
                "client.serverUrl",
            ),
            (
                json!({"version": 1, "client": {"unknown": true}}),
                "client.unknown",
            ),
        ];
        assert_invalid_configs(&invalid);
    }

    #[test]
    fn global_config_rejects_invalid_server_fields() {
        let invalid = [
            (json!({"version": 1, "server": []}), "server"),
            (
                json!({"version": 1, "server": {"host": "  "}}),
                "server.host",
            ),
            (json!({"version": 1, "server": {"port": 0}}), "server.port"),
            (json!({"version": 1, "server": {"port": -1}}), "server.port"),
            (
                json!({"version": 1, "server": {"port": 1.5}}),
                "server.port",
            ),
            (
                json!({"version": 1, "server": {"port": 65_536}}),
                "server.port",
            ),
            (
                json!({"version": 1, "server": {"port": "8123"}}),
                "server.port",
            ),
            (
                json!({"version": 1, "server": {"port": null}}),
                "server.port",
            ),
            (
                json!({"version": 1, "server": {"unknown": true}}),
                "server.unknown",
            ),
        ];
        assert_invalid_configs(&invalid);
    }

    fn assert_invalid_configs(invalid: &[(Value, &str)]) {
        for (value, field) in invalid {
            let error = parse_global_config(value).expect_err(field);
            assert!(
                error.message().contains(field),
                "{field}: {}",
                error.message()
            );
            assert!(!error.message().contains("12345"));
        }
    }

    #[test]
    fn global_config_normalizes_strings_and_omits_empty_sections() {
        let value = parse_global_config(&json!({
            "version": 1.0,
            "defaults": {"embedding": " local/model ", "modelCacheDir": " cache "},
            "providers": {"qwen": {"apiKey": " secret "}, "unused": {}},
            "models": {
                "qwen/model": {"endpoint": " https://example.test/embeddings "},
                "local/model": {"device": "cpu"},
                "local/empty": {}
            },
            "client": {"mode": "server", "serverUrl": " custom-url "},
            "server": {"host": " ::1 ", "port": 8123.0},
            "log": {}
        }))
        .expect("valid global config");
        assert_eq!(
            value,
            json!({
                "version": 1,
                "defaults": {"embedding": "local/model", "modelCacheDir": "cache"},
                "providers": {"qwen": {"apiKey": "secret"}},
                "models": {
                    "qwen/model": {"endpoint": "https://example.test/embeddings"},
                    "local/model": {"device": "cpu"}
                },
                "client": {"mode": "server", "serverUrl": "custom-url"},
                "server": {"host": "::1", "port": 8123}
            })
        );
        assert_eq!(listen_from_config(&value).expect("listen"), "[::1]:8123");
        assert_eq!(server_url_from_config(&value).expect("url"), "custom-url");
    }

    #[test]
    fn configured_server_url_uses_listen_when_client_url_is_absent() {
        let defaults = parse_global_config(&json!({"version": 1})).expect("default config");
        assert_eq!(
            listen_from_config(&defaults).expect("listen"),
            "127.0.0.1:7999"
        );
        assert_eq!(
            server_url_from_config(&defaults).expect("url"),
            "http://127.0.0.1:7999/mcp"
        );

        let ipv6 =
            parse_global_config(&json!({"version": 1, "server": {"host": "::1", "port": 8123}}))
                .expect("IPv6 config");
        assert_eq!(
            server_url_from_config(&ipv6).expect("url"),
            "http://[::1]:8123/mcp"
        );

        let bracketed =
            parse_global_config(&json!({"version": 1, "server": {"host": "[::1]", "port": 8123}}))
                .expect("bracketed IPv6 config");
        assert_eq!(
            listen_from_config(&bracketed).expect("bracketed listen"),
            "[::1]:8123"
        );
        assert_eq!(
            server_url_from_config(&bracketed).expect("bracketed URL"),
            "http://[::1]:8123/mcp"
        );

        let localhost = parse_global_config(
            &json!({"version": 1, "server": {"host": "LOCALHOST", "port": 8124}}),
        )
        .expect("localhost config");
        assert_eq!(
            listen_from_config(&localhost).expect("listen"),
            "localhost:8124"
        );

        let invalid = parse_global_config(&json!({"version": 1, "server": {"host": "0.0.0.0"}}))
            .expect("host passes schema validation");
        assert!(listen_from_config(&invalid).is_err());
        let explicit_url = parse_global_config(&json!({"version": 1, "client": {"serverUrl": "http://server.test/mcp"}, "server": {"host": "0.0.0.0"}}))
            .expect("client URL config");
        assert_eq!(
            server_url_from_config(&explicit_url).expect("client URL"),
            "http://server.test/mcp"
        );
    }

    #[test]
    fn daemon_log_settings_match_the_global_config_contract() {
        let directory = tempfile::tempdir().expect("config directory");
        let path = directory.path().join("config.json");
        for log in [
            json!({"maxBytes": 0}),
            json!({"keep": -1}),
            json!({"keep": 9_007_199_254_740_992_u64}),
            json!({"level": "trace"}),
            json!({"unknown": true}),
        ] {
            fs::write(
                &path,
                serde_json::to_vec(&json!({"version": 1, "log": log})).expect("json"),
            )
            .expect("configuration");
            assert!(read_at(&path).is_err());
        }
        fs::write(
            &path,
            br#"{"version":1,"log":{"maxBytes":1024,"keep":0,"level":"debug"}}"#,
        )
        .expect("configuration");
        assert!(read_at(&path).is_ok());
        fs::write(&path, br#"{"version":1,"log":{"maxBytes":1.0,"keep":0e0}}"#)
            .expect("configuration");
        assert_eq!(
            read_at(&path).expect("integral numbers")["log"]["maxBytes"],
            1
        );
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
