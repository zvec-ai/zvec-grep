//! Configuration evidence is cached for one resolution sweep. An invalid
//! nearest config is authoritative; it must not expose a parent config.
use std::sync::Arc;

use serde_json::{Map, Value};

use super::{RuleCaches, importing_dir, join_specifier};

#[derive(Clone, Default)]
pub(super) struct TsConfig {
    pub base_url: Option<String>,
    pub paths: Map<String, Value>,
    pub paths_directory: String,
}

pub(super) fn read(caches: &mut RuleCaches, path: &str) -> Result<Option<Arc<Value>>, ()> {
    let Some(root) = &caches.source_root else {
        return Ok(None);
    };
    caches
        .module_configs
        .entry(path.to_owned())
        .or_insert_with(|| {
            let source = match std::fs::read_to_string(root.join(path)) {
                Ok(source) => source,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    tracing::warn!(path, %error, "cannot read module configuration");
                    return Err(());
                }
            };
            let options = jsonc_parser::ParseOptions {
                allow_comments: true,
                allow_trailing_commas: true,
                allow_loose_object_property_names: false,
                allow_missing_commas: false,
                allow_single_quoted_strings: false,
                allow_hexadecimal_numbers: false,
                allow_unary_plus_numbers: false,
                allow_bare_decimal_point_numbers: false,
                allow_non_finite_numbers: false,
                allow_extended_string_escapes: false,
            };
            match jsonc_parser::parse_to_serde_value::<Value>(&source, &options) {
                Ok(value) if value.is_object() => Ok(Some(Arc::new(value))),
                Ok(_) => {
                    tracing::warn!(path, "module configuration must be an object");
                    Err(())
                }
                Err(error) => {
                    tracing::warn!(path, %error, "invalid module configuration");
                    Err(())
                }
            }
        })
        .clone()
}

pub(super) fn typescript(caches: &mut RuleCaches, path: &str) -> Result<Option<Arc<TsConfig>>, ()> {
    typescript_inner(caches, path, 32)
}

fn typescript_inner(
    caches: &mut RuleCaches,
    path: &str,
    remaining: usize,
) -> Result<Option<Arc<TsConfig>>, ()> {
    if remaining == 0 {
        return Err(());
    }
    if let Some(config) = caches.tsconfigs.get(path) {
        return config.clone();
    }
    // A cycle re-enters this sentinel and fails instead of recursing forever.
    caches.tsconfigs.insert(path.to_owned(), Err(()));
    let result = load_typescript(caches, path, remaining - 1).map(|config| config.map(Arc::new));
    caches.tsconfigs.insert(path.to_owned(), result.clone());
    result
}

fn load_typescript(
    caches: &mut RuleCaches,
    path: &str,
    remaining: usize,
) -> Result<Option<TsConfig>, ()> {
    let Some(value) = read(caches, path)? else {
        return Ok(None);
    };
    let directory = importing_dir(path);
    let mut config = if let Some(extends) = value.get("extends") {
        let extends = extends.as_str().ok_or(())?;
        // Package-based inheritance requires package resolution; never guess.
        if !extends.starts_with("./") && !extends.starts_with("../") {
            return Err(());
        }
        let base = join_specifier(directory, extends).ok_or(())?;
        match typescript_inner(caches, &base, remaining)? {
            Some(config) => (*config).clone(),
            None if !std::path::Path::new(&base)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json")) =>
            {
                typescript_inner(caches, &format!("{base}.json"), remaining)?
                    .ok_or(())?
                    .as_ref()
                    .clone()
            }
            None => return Err(()),
        }
    } else {
        TsConfig::default()
    };
    if let Some(compiler) = value.get("compilerOptions") {
        let compiler = compiler.as_object().ok_or(())?;
        if let Some(base_url) = compiler.get("baseUrl") {
            let base_url = base_url.as_str().ok_or(())?;
            // The marker lets join_specifier also normalize '.' or '..'.
            let joined = join_specifier(directory, &format!("{base_url}/__base__")).ok_or(())?;
            config.base_url = Some(importing_dir(&joined).to_owned());
        }
        if let Some(paths) = compiler.get("paths") {
            config.paths.clone_from(paths.as_object().ok_or(())?);
            directory.clone_into(&mut config.paths_directory);
        }
    }
    Ok(Some(config))
}
