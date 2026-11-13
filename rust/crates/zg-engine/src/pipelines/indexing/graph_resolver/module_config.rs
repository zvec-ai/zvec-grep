//! Configuration evidence is cached for one resolution sweep. An invalid
//! nearest config is authoritative; it must not expose a parent config.
#[cfg(test)]
mod tests;

use std::sync::Arc;

use serde_json::{Map, Value};

use super::{RuleCaches, importing_dir, join_specifier};

#[derive(Clone, Default)]
pub(super) struct TsConfig {
    pub base_url: Option<String>,
    // None inherits earlier mappings; Some(empty) explicitly clears them.
    pub paths: Option<Map<String, Value>>,
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
    let own = compiler_options(&value, directory)?;
    // Both fields are fully specified here: no parent can change how these
    // mappings resolve, even when package-based inheritance is unavailable.
    if own.base_url.is_some() && own.paths.is_some() {
        return Ok(Some(own));
    }
    let bases = match value.get("extends") {
        None => &[][..],
        Some(value @ Value::String(_)) => std::slice::from_ref(value),
        Some(Value::Array(bases)) => bases.as_slice(),
        Some(_) => return Err(()),
    };
    let mut config = TsConfig::default();
    // Each later base overrides only the options it defines. In particular,
    // paths is replaced as a whole rather than merging individual aliases.
    for base in bases {
        let inherited = relative_base(caches, directory, base.as_str().ok_or(())?, remaining)?;
        config.overlay(&inherited);
    }
    config.overlay(&own);
    Ok(Some(config))
}

fn relative_base(
    caches: &mut RuleCaches,
    directory: &str,
    extends: &str,
    remaining: usize,
) -> Result<Arc<TsConfig>, ()> {
    // Package-based inheritance requires package resolution; never guess.
    if !extends.starts_with("./") && !extends.starts_with("../") {
        return Err(());
    }
    let base = join_specifier(directory, extends).ok_or(())?;
    match typescript_inner(caches, &base, remaining)? {
        Some(config) => Ok(config),
        None if !std::path::Path::new(&base)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json")) =>
        {
            typescript_inner(caches, &format!("{base}.json"), remaining)?.ok_or(())
        }
        None => Err(()),
    }
}

impl TsConfig {
    fn overlay(&mut self, next: &Self) {
        if next.base_url.is_some() {
            self.base_url.clone_from(&next.base_url);
        }
        if next.paths.is_some() {
            self.paths.clone_from(&next.paths);
            self.paths_directory.clone_from(&next.paths_directory);
        }
    }
}

fn compiler_options(value: &Value, directory: &str) -> Result<TsConfig, ()> {
    let mut config = TsConfig::default();
    if let Some(compiler) = value.get("compilerOptions") {
        let compiler = compiler.as_object().ok_or(())?;
        if let Some(base_url) = compiler.get("baseUrl") {
            let base_url = base_url.as_str().ok_or(())?;
            // The marker lets join_specifier also normalize '.' or '..'.
            let joined = join_specifier(directory, &format!("{base_url}/__base__")).ok_or(())?;
            config.base_url = Some(importing_dir(&joined).to_owned());
        }
        if let Some(paths) = compiler.get("paths") {
            config.paths = Some(paths.as_object().ok_or(())?.clone());
            directory.clone_into(&mut config.paths_directory);
        }
    }
    Ok(config)
}
