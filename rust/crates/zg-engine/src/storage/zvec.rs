//! Shared zvec initialization and collection operations.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::OnceLock,
};
use zvec_rust::{
    Collection, CollectionOptions, CollectionSchema, DataType, Doc, FieldSchema, IndexParams,
};

use crate::{EngineError, EngineResult};

pub(super) const WRITE_BATCH: usize = 1024;
static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();

pub(super) fn initialize() -> EngineResult<()> {
    match INITIALIZED.get_or_init(|| {
        let mut config = zvec_rust::ConfigBuilder::new()
            .num_threads(2)
            .memory_limit(512 * 1024 * 1024);
        let executable = std::env::current_exe()
            .map_err(|error| format!("cannot locate executable for jieba dictionaries: {error}"))?;
        let environment_dir = std::env::var_os("ZVEC_JIEBA_DICT_DIR").filter(|dir| !dir.is_empty());
        let default_dir = zvec_rust::get_default_jieba_dict_dir();
        if let Some(dir) = jieba_dict_dir(
            &executable,
            environment_dir.as_deref().map(Path::new),
            (!default_dir.is_empty()).then(|| Path::new(&default_dir)),
        )? {
            config = config.jieba_dict_dir(native_path(&dir).map_err(|error| error.to_string())?);
        }
        zvec_rust::initialize(Some(&config.build())).map_err(|error| error.to_string())
    }) {
        Ok(()) => Ok(()),
        Err(message) => Err(EngineError::storage_failure(format!(
            "cannot initialize zvec: {message}"
        ))),
    }
}

fn jieba_dict_dir(
    executable: &Path,
    environment_dir: Option<&Path>,
    default_dir: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    // Preserve explicit application/user configuration without mutating the
    // process environment (other engine instances may be running concurrently).
    if let Some(dir) = environment_dir.or(default_dir) {
        return require_jieba_dict_dir(dir).map(Some);
    }
    let Some(executable_dir) = executable.parent() else {
        return Ok(None);
    };
    let is_cli = executable
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("zg") || name.eq_ignore_ascii_case("zg.exe"));
    if is_cli && executable_dir.file_name().is_some_and(|name| name == "bin") {
        // Windows delay-loaded imports can resolve to a thunk in zg.exe when
        // zvec tries to locate its DLL. Use our bin/lib package layout directly,
        // and never let a build-machine SDK cache hide an incomplete package.
        let package_dir = executable_dir.parent().unwrap_or(executable_dir);
        return require_jieba_dict_dir(&package_dir.join("lib/data/jieba_dict")).map(Some);
    }
    let adjacent = executable_dir.join("data/jieba_dict");
    if adjacent.exists() {
        return require_jieba_dict_dir(&adjacent).map(Some);
    }
    // Cargo stages assets in target/<profile>, also for tests and examples.
    if executable_dir
        .file_name()
        .is_some_and(|name| name == "deps" || name == "examples")
        && let Some(profile_dir) = executable_dir.parent()
    {
        let staged = profile_dir.join("data/jieba_dict");
        if staged.exists() {
            return require_jieba_dict_dir(&staged).map(Some);
        }
    }
    // Development/embedded callers without staged assets retain the SDK's
    // existing discovery of its linked library or build-time dictionary path.
    Ok(None)
}

fn require_jieba_dict_dir(dir: &Path) -> Result<PathBuf, String> {
    for name in ["jieba.dict.utf8", "hmm_model.utf8"] {
        if !dir.join(name).is_file() {
            return Err(format!(
                "jieba dictionary directory {} is missing required file {name}",
                dir.display()
            ));
        }
    }
    Ok(dir.to_path_buf())
}

pub(super) fn open_collection(
    path: &Path,
    schema: &CollectionSchema,
    read_only: bool,
) -> EngineResult<Collection> {
    let text = native_path(path)?;
    let mut options = native(CollectionOptions::new(), "configure collection")?;
    native(
        options.set_read_only(read_only),
        "set collection access mode",
    )?;
    if path.exists() {
        native(
            Collection::open(text, Some(&options)),
            &format!("open collection {}", path.display()),
        )
    } else if read_only {
        Err(EngineError::not_found(format!(
            "index collection does not exist: {}",
            path.display()
        )))
    } else {
        native(
            Collection::create_and_open(text, schema, Some(&options)),
            &format!("create collection {}", path.display()),
        )
    }
}

fn native_path(path: &Path) -> EngineResult<&str> {
    // zvec rejects the `?` in Windows verbatim prefixes. Simplify only when
    // the regular path identifies the same location; retain internal paths.
    #[cfg(windows)]
    let path = dunce::simplified(path);
    path.to_str().ok_or_else(|| {
        EngineError::invalid_argument(format!(
            "zvec storage path must be UTF-8: {}",
            path.display()
        ))
    })
}

pub(super) fn scalar(
    schema: &mut CollectionSchema,
    name: &str,
    data_type: DataType,
    nullable: bool,
    indexed: bool,
) -> EngineResult<()> {
    let mut field = native(
        FieldSchema::new(name, data_type, nullable, 0),
        "define scalar field",
    )?;
    if indexed {
        native(
            field.set_index_params(&native(
                IndexParams::invert(true, false),
                "define scalar index",
            )?),
            "attach scalar index",
        )?;
    }
    native(schema.add_field(&field), "add scalar field")
}

pub(super) fn wildcard_string(
    schema: &mut CollectionSchema,
    name: &str,
    nullable: bool,
) -> EngineResult<()> {
    let mut field = native(
        FieldSchema::new(name, DataType::String, nullable, 0),
        "define indexed path string",
    )?;
    native(
        field.set_index_params(&native(
            IndexParams::invert(false, true),
            "define path string index",
        )?),
        "attach path string index",
    )?;
    native(schema.add_field(&field), "add indexed path string")
}

pub(super) fn fetch_map(
    collection: &Collection,
    keys: &[String],
) -> EngineResult<HashMap<String, Doc>> {
    let mut result = HashMap::with_capacity(keys.len());
    for batch in keys.chunks(WRITE_BATCH) {
        let refs = batch.iter().map(String::as_str).collect::<Vec<_>>();
        for doc in native(
            collection.fetch_with_options(&refs, None, false),
            "fetch stored records",
        )? {
            result.insert(doc_key(&doc)?.to_owned(), doc);
        }
    }
    Ok(result)
}

pub(super) fn write_docs(
    collection: &Collection,
    docs: &[Doc],
    operation: &str,
) -> EngineResult<()> {
    for batch in docs.chunks(WRITE_BATCH) {
        let refs = batch.iter().collect::<Vec<_>>();
        let result = native(collection.upsert(&refs), operation)?;
        if result.results.len() != batch.len()
            || result.error_count != 0
            || result.success_count != u64::try_from(batch.len()).unwrap_or(u64::MAX)
            || result.results.iter().any(|status| !status.success)
        {
            let detail = result
                .results
                .iter()
                .find(|status| !status.success)
                .map_or_else(
                    || "incomplete write result".to_owned(),
                    |status| format!("{}: {}", status.code, status.message),
                );
            return Err(EngineError::storage_failure(format!(
                "{operation}: {detail}"
            )));
        }
    }
    Ok(())
}

pub(super) fn u32_field(doc: &Doc, name: &str) -> EngineResult<u32> {
    native(doc.get_u32(name), "read numeric field")?.ok_or_else(|| corrupt("missing numeric field"))
}

pub(super) fn doc_key(doc: &Doc) -> EngineResult<&str> {
    doc.get_pk()
        .ok_or_else(|| corrupt("stored record has no primary key"))
}

pub(super) fn string_field(doc: &Doc, field: &str) -> EngineResult<String> {
    native(doc.get_string(field), "read stored field")?
        .ok_or_else(|| corrupt(format!("stored field {field} is missing")))
}

pub(super) fn corrupt(message: impl std::fmt::Display) -> EngineError {
    EngineError::storage_failure(message.to_string())
}
#[track_caller]
pub(super) fn native<T>(result: zvec_rust::Result<T>, operation: &str) -> EngineResult<T> {
    result.map_err(|error| EngineError::storage_failure(format!("zvec {operation}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dictionaries(dir: &Path) {
        std::fs::create_dir_all(dir).expect("dictionary directory");
        for name in ["jieba.dict.utf8", "hmm_model.utf8"] {
            std::fs::write(dir.join(name), "fixture").expect("dictionary file");
        }
    }

    #[test]
    fn packaged_dictionary_uses_sibling_lib_with_spaces_and_unicode() {
        let root = tempfile::tempdir().expect("temporary directory");
        let package = root.path().join("包 with spaces");
        let dir = package.join("lib/data/jieba_dict");
        dictionaries(&dir);
        for binary in ["zg", "zg.exe"] {
            assert_eq!(
                jieba_dict_dir(&package.join("bin").join(binary), None, None)
                    .expect("packaged dictionary"),
                Some(dir.clone())
            );
        }
    }

    #[test]
    fn packaged_dictionary_rejects_missing_files_without_sdk_fallback() {
        let root = tempfile::tempdir().expect("temporary directory");
        let executable = root.path().join("bin/zg.exe");
        let dir = root.path().join("lib/data/jieba_dict");
        let error = jieba_dict_dir(&executable, None, None).expect_err("absent dictionaries");
        assert!(error.contains("jieba.dict.utf8"));
        std::fs::create_dir_all(&dir).expect("dictionary directory");
        std::fs::write(dir.join("jieba.dict.utf8"), "fixture").expect("dictionary file");
        let error = jieba_dict_dir(&executable, None, None).expect_err("absent HMM model");
        assert!(error.contains("hmm_model.utf8"));
        assert!(error.contains(&dir.display().to_string()));
    }

    #[test]
    fn explicit_dictionary_overrides_default_and_package_discovery() {
        let root = tempfile::tempdir().expect("temporary directory");
        let environment_dir = root.path().join("environment");
        let default_dir = root.path().join("default");
        dictionaries(&environment_dir);
        dictionaries(&default_dir);
        let executable = root.path().join("broken-package/bin/zg.exe");
        assert_eq!(
            jieba_dict_dir(&executable, Some(&environment_dir), Some(&default_dir))
                .expect("environment override"),
            Some(environment_dir)
        );
        assert_eq!(
            jieba_dict_dir(&executable, None, Some(&default_dir)).expect("default override"),
            Some(default_dir.clone())
        );
        assert!(
            jieba_dict_dir(
                &executable,
                Some(&root.path().join("missing")),
                Some(&default_dir)
            )
            .is_err()
        );
    }

    #[test]
    fn cargo_dictionary_layout_supports_binaries_tests_and_examples() {
        let root = tempfile::tempdir().expect("temporary directory");
        let profile = root.path().join("target/debug");
        let dir = profile.join("data/jieba_dict");
        dictionaries(&dir);
        for binary in ["zg.exe", "deps/zg-engine-test.exe", "examples/example.exe"] {
            assert_eq!(
                jieba_dict_dir(&profile.join(binary), None, None).expect("Cargo dictionary"),
                Some(dir.clone())
            );
        }
    }

    #[test]
    fn unstaged_development_dictionary_retains_sdk_discovery() {
        let root = tempfile::tempdir().expect("temporary directory");
        for binary in ["target/debug/deps/test", "venv/bin/embedded-app.exe"] {
            assert_eq!(
                jieba_dict_dir(&root.path().join(binary), None, None).expect("SDK discovery"),
                None
            );
        }
    }
}
