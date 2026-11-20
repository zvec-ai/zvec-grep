//! Resolve named FTS presets into concrete updates before transport or persistence.

use std::{
    collections::BTreeMap,
    fmt, fs,
    path::{Path, PathBuf},
};

use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};
use serde_json::{Map, Value};

use crate::{
    EngineError, EngineResult,
    api::index::{FtsConfig, FtsConfigUpdate},
};

/// An effective language preset and its optional user-file source.
#[derive(Clone, Debug)]
pub struct LanguagePreset {
    pub name: String,
    pub config: FtsConfig,
    /// None identifies a built-in preset.
    pub source: Option<PathBuf>,
}

/// Lists effective presets, including user overrides, in name order.
/// # Errors
/// Returns file access, format, or analysis validation errors.
pub fn list() -> EngineResult<Vec<LanguagePreset>> {
    Ok(load_at(&mapping_path()?)?.into_values().collect())
}

/// Expands a selected preset, then replaces fields supplied explicitly.
/// Without a name, preserves the update and does not read the mapping file.
/// # Errors
/// Returns an error for unknown presets, invalid files, or invalid overrides.
pub fn resolve(
    language: Option<&str>,
    overrides: FtsConfigUpdate,
) -> EngineResult<FtsConfigUpdate> {
    match language {
        Some(name) => resolve_at(&mapping_path()?, name, &overrides),
        None => Ok(overrides),
    }
}

fn mapping_path() -> EngineResult<PathBuf> {
    Ok(super::global_config_path()?.with_file_name("fts-languages.json"))
}

fn resolve_at(
    path: &Path,
    name: &str,
    overrides: &FtsConfigUpdate,
) -> EngineResult<FtsConfigUpdate> {
    let presets = load_at(path)?;
    let name = name.trim().to_ascii_lowercase();
    let preset = presets.get(&name).ok_or_else(|| {
        EngineError::invalid_argument(format!(
            "unknown FTS language {name:?}; available: {}",
            presets
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    let config = overrides.apply(&preset.config)?;
    Ok(FtsConfigUpdate {
        tokenizer: Some(config.tokenizer),
        filters: Some(config.filters),
        extra_params: Some(config.extra_params),
    })
}

pub(crate) fn builtins() -> BTreeMap<String, LanguagePreset> {
    let mut presets = BTreeMap::new();
    // Use canonical Snowball algorithms, with ISO 639-1 names for preset selection.
    for (name, language) in [
        ("ar", "arabic"),
        ("ca", "catalan"),
        ("cs", "czech"),
        ("da", "danish"),
        ("de", "german"),
        ("el", "greek"),
        ("en", "english"),
        ("eo", "esperanto"),
        ("es", "spanish"),
        ("et", "estonian"),
        ("eu", "basque"),
        ("fa", "persian"),
        ("fi", "finnish"),
        ("fr", "french"),
        ("ga", "irish"),
        ("hi", "hindi"),
        ("hu", "hungarian"),
        ("hy", "armenian"),
        ("id", "indonesian"),
        ("it", "italian"),
        ("lt", "lithuanian"),
        ("ne", "nepali"),
        ("nl", "dutch"),
        ("no", "norwegian"),
        ("pl", "polish"),
        ("pt", "portuguese"),
        ("ro", "romanian"),
        ("ru", "russian"),
        ("sr", "serbian"),
        ("st", "sesotho"),
        ("sv", "swedish"),
        ("ta", "tamil"),
        ("tr", "turkish"),
        ("yi", "yiddish"),
    ] {
        presets.insert(
            name.to_owned(),
            LanguagePreset {
                name: name.to_owned(),
                config: FtsConfig {
                    tokenizer: "standard".into(),
                    filters: vec!["lowercase".into(), "stemmer".into()],
                    extra_params: Map::from_iter([("stemmer_lang".into(), Value::from(language))]),
                },
                source: None,
            },
        );
    }
    presets.insert(
        "zh".into(),
        LanguagePreset {
            name: "zh".into(),
            config: FtsConfig::default(),
            source: None,
        },
    );
    presets
}

fn load_at(path: &Path) -> EngineResult<BTreeMap<String, LanguagePreset>> {
    let mut presets = builtins();
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(presets),
        Err(error) => {
            return Err(EngineError::from_io(
                format!("read FTS language mappings '{}'", path.display()),
                &error,
            ));
        }
    };
    let file: LanguageFile =
        serde_json::from_slice(&bytes).map_err(|error| invalid_file(path, error))?;
    if file.version != 1 {
        return Err(invalid_file(path, "version must be 1"));
    }
    for (name, definition) in file.languages {
        if !valid_name(&name) {
            return Err(invalid_file(
                path,
                format!(
                    "invalid language name {name:?}; use 1-64 lowercase ASCII letters, digits, hyphens, or underscores, starting with a letter"
                ),
            ));
        }
        let config = FtsConfig {
            tokenizer: definition.tokenizer,
            filters: definition.filters,
            extra_params: definition.extra_params,
        };
        config
            .validate()
            .map_err(|error| invalid_file(path, format!("language {name:?}: {error}")))?;
        // A user definition replaces the complete preset, including empty fields.
        presets.insert(
            name.clone(),
            LanguagePreset {
                name,
                config,
                source: Some(path.to_owned()),
            },
        );
    }
    Ok(presets)
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn invalid_file(path: &Path, message: impl fmt::Display) -> EngineError {
    EngineError::invalid_argument(format!(
        "invalid FTS language file '{}': {message}",
        path.display()
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LanguageFile {
    version: u32,
    #[serde(deserialize_with = "unique_languages")]
    languages: BTreeMap<String, CompleteConfig>,
}

// Every field is required so an override cannot inherit part of a built-in preset.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompleteConfig {
    tokenizer: String,
    filters: Vec<String>,
    extra_params: Map<String, Value>,
}

fn unique_languages<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, CompleteConfig>, D::Error> {
    struct Entries;

    impl<'de> Visitor<'de> for Entries {
        type Value = BTreeMap<String, CompleteConfig>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object with unique language names")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut entries = BTreeMap::new();
            while let Some(name) = map.next_key::<String>()? {
                if entries.contains_key(&name) {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate language name {name:?}"
                    )));
                }
                let config = map.next_value::<CompleteConfig>().map_err(|error| {
                    serde::de::Error::custom(format!("language {name:?}: {error}"))
                })?;
                entries.insert(name, config);
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(Entries)
}

#[cfg(test)]
mod tests;
