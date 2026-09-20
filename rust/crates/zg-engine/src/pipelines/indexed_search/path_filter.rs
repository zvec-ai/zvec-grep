//! Compile path globs into storage predicates only when they preserve file-selection semantics.

use std::path::Path;

use crate::{
    EngineError,
    domain::{GlobRule, SourcePath},
    storage::spi::{StoragePathFilter, WorkspaceIndexStorage},
};

pub(super) fn compile_path_filter(
    rules: &[GlobRule],
    storage: &dyn WorkspaceIndexStorage,
) -> Result<Option<StoragePathFilter>, EngineError> {
    if !storage.supports_path_filters() || rules.iter().any(|rule| rule.case_insensitive) {
        return Ok(None);
    }
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut has_exclusion = false;
    let mut uses_file_name = false;
    for rule in rules {
        let pattern = rule.pattern.as_str();
        if let Some(pattern) = pattern.strip_prefix('!') {
            // Other exclusions can prune matching parent directories and need
            // the shared matcher's walk-aware fallback.
            let Some(directory) = recursive_directory(pattern) else {
                return Ok(None);
            };
            excludes.push(directory_filter(directory)?);
            has_exclusion = true;
        } else {
            // Re-inclusion depends on whether parent directories remain reachable.
            if has_exclusion {
                return Ok(None);
            }
            let Some((predicate, name)) = positive_filter(pattern)? else {
                return Ok(None);
            };
            uses_file_name |= name;
            includes.push(predicate);
        }
    }
    if uses_file_name && storage.has_non_unicode_file_names()? {
        // Lossy string metadata cannot decide matches for native file names.
        return Ok(None);
    }
    let include = if includes.is_empty() {
        StoragePathFilter::All
    } else {
        any(includes)
    };
    Ok(Some(if excludes.is_empty() {
        include
    } else {
        all(vec![include, negate(any(excludes))])
    }))
}

fn positive_filter(pattern: &str) -> Result<Option<(StoragePathFilter, bool)>, EngineError> {
    if matches!(pattern, "*" | "**" | "**/*") {
        return Ok(Some((StoragePathFilter::All, false)));
    }
    if let Some(directory) = recursive_directory(pattern) {
        return Ok(Some((directory_filter(directory)?, false)));
    }
    // **/foo and foo both match basenames at any depth.
    let unanchored = pattern.strip_prefix("**/").unwrap_or(pattern);
    if let Some(predicate) = file_name_filter(unanchored) {
        return Ok(Some((predicate, true)));
    }
    if let Some((directory, basename)) = pattern.split_once("/**/")
        && literal_directory(directory)
        && let Some(predicate) = file_name_filter(basename)
    {
        return Ok(Some((
            all(vec![directory_filter(directory)?, predicate]),
            true,
        )));
    }
    Ok(None)
}

fn recursive_directory(pattern: &str) -> Option<&str> {
    let directory = pattern.strip_suffix("/**")?;
    literal_directory(directory).then_some(directory)
}

fn literal_directory(directory: &str) -> bool {
    let directory = directory.strip_prefix('/').unwrap_or(directory);
    !directory.is_empty()
        && directory
            .split('/')
            .all(|part| literal_name(part) && !matches!(part, "." | ".."))
}

fn directory_filter(directory: &str) -> Result<StoragePathFilter, EngineError> {
    let directory = directory.strip_prefix('/').unwrap_or(directory);
    Ok(StoragePathFilter::Directory(SourcePath::new(Path::new(
        directory,
    ))?))
}

fn file_name_filter(pattern: &str) -> Option<StoragePathFilter> {
    if literal_name(pattern) {
        return Some(StoragePathFilter::FileNameExact(pattern.to_owned()));
    }
    if let Some(prefix) = pattern
        .strip_suffix('*')
        .filter(|prefix| literal_name(prefix))
    {
        return Some(StoragePathFilter::FileNamePrefix(prefix.to_owned()));
    }
    if let Some(suffix) = pattern
        .strip_prefix('*')
        .filter(|suffix| literal_name(suffix))
    {
        return Some(StoragePathFilter::FileNameSuffix(suffix.to_owned()));
    }
    None
}

fn literal_name(name: &str) -> bool {
    // A conservative alphabet avoids differences between glob escaping and
    // platform separators.
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

pub(super) fn any(predicates: Vec<StoragePathFilter>) -> StoragePathFilter {
    let mut result = Vec::new();
    for predicate in predicates {
        match predicate {
            StoragePathFilter::All => return StoragePathFilter::All,
            StoragePathFilter::None => {}
            predicate => result.push(predicate),
        }
    }
    match result.len() {
        0 => StoragePathFilter::None,
        1 => result.pop().expect("one predicate"),
        _ => StoragePathFilter::Or(result),
    }
}

pub(super) fn all(predicates: Vec<StoragePathFilter>) -> StoragePathFilter {
    let mut result = Vec::new();
    for predicate in predicates {
        match predicate {
            StoragePathFilter::None => return StoragePathFilter::None,
            StoragePathFilter::All => {}
            predicate => result.push(predicate),
        }
    }
    match result.len() {
        0 => StoragePathFilter::All,
        1 => result.pop().expect("one predicate"),
        _ => StoragePathFilter::And(result),
    }
}

pub(super) fn negate(predicate: StoragePathFilter) -> StoragePathFilter {
    match predicate {
        StoragePathFilter::All => StoragePathFilter::None,
        StoragePathFilter::None => StoragePathFilter::All,
        StoragePathFilter::Not(predicate) => *predicate,
        predicate => StoragePathFilter::Not(Box::new(predicate)),
    }
}
