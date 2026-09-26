use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};
use zg_host_native::{HostError, PathPolicy, RootSpec};

use super::GlobMatcher;
use crate::{EngineError, EngineResult, domain::ScanRules};

const MAX_IGNORE_BYTES: u64 = 1_048_576;
const MAX_IGNORE_RULES: usize = 16_384;
const MAX_CACHED_IGNORE_FILES: usize = 4_096;
const MAX_CACHED_IGNORE_BYTES: usize = 4 * 1_048_576;
const MAX_CACHED_DIRECTORIES: usize = 4_096;
const IGNORE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".rgignore"];

/// Workspace policy used while walking and registering filesystem watches.
#[derive(Debug)]
pub(crate) struct ScanPolicy {
    root: PathBuf,
    options: ScanRules,
    matcher: GlobMatcher,
    types: ignore::types::Types,
    defaults: Gitignore,
    control_paths: Mutex<Vec<PathBuf>>,
    cache: Mutex<IgnoreCache>,
    repositories: Mutex<HashMap<PathBuf, bool>>,
}

#[derive(Debug, Default)]
struct IgnoreCache {
    entries: HashMap<PathBuf, Arc<Gitignore>>,
    source_bytes: usize,
}

impl ScanPolicy {
    pub(crate) fn new(root: &Path, options: &ScanRules) -> EngineResult<Self> {
        let matcher = GlobMatcher::new(root, &options.globs)?;
        let mut options = options.clone();
        options.ignore_files = options
            .ignore_files
            .iter()
            .map(|path| control_alias(&root.join(path)))
            .collect();
        let mut defaults = GitignoreBuilder::new(root);
        for name in DEFAULT_IGNORED_DIRECTORY_NAMES {
            defaults
                .add_line(None, &format!("{name}/"))
                .map_err(|error| ignore_error(&error))?;
        }
        for pattern in DEFAULT_IGNORED_FILE_PATTERNS {
            defaults
                .add_line(None, pattern)
                .map_err(|error| ignore_error(&error))?;
        }
        let control_paths = control_paths(root, &options);
        let types = super::file_types(&options.file_types, &options.excluded_file_types)?;
        Ok(Self {
            root: root.to_path_buf(),
            options,
            matcher,
            types,
            defaults: defaults.build().map_err(|error| ignore_error(&error))?,
            control_paths: Mutex::new(control_paths),
            cache: Mutex::new(IgnoreCache::default()),
            repositories: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn root_spec(root: &Path, options: &ScanRules) -> EngineResult<RootSpec> {
        let policy = Arc::new(Self::new(root, options)?);
        let mut spec = RootSpec::new(root.to_path_buf(), policy);
        spec.follow = options.follow_symlinks;
        spec.max_depth = options.max_depth;
        spec.max_file_size_bytes = options.max_file_size_bytes;
        Ok(spec)
    }

    fn selected(&self, path: &Path, directory: bool) -> Result<bool, HostError> {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return Ok(false);
        };
        if relative.as_os_str().is_empty() {
            return Ok(true);
        }
        if relative
            .components()
            .any(|part| matches!(part.as_os_str().to_str(), Some(".git" | ".zvec-grep")))
        {
            return Ok(false);
        }
        // Direct incremental scans obey the same ancestor boundaries as full walks.
        for parent in relative
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .take_while(|parent| !parent.as_os_str().is_empty())
        {
            if !self.selected_entry(&self.root.join(parent), true)? {
                return Ok(false);
            }
        }
        self.selected_entry(path, directory)
    }

    fn selected_entry(&self, absolute: &Path, directory: bool) -> Result<bool, HostError> {
        if !self.matches_rules(absolute, directory)? {
            return Ok(false);
        }
        // Ordinary exclusions need no marker checks or extra control watches.
        Ok(!directory || self.options.nested_git || !self.is_nested_repository(absolute)?)
    }

    fn matches_rules(&self, absolute: &Path, directory: bool) -> Result<bool, HostError> {
        let relative = absolute.strip_prefix(&self.root).unwrap_or(absolute);
        if !directory && self.types.matched(relative, false).is_ignore() {
            return Ok(false);
        }
        match self.matcher.path_match(relative, directory) {
            Match::Ignore(()) => return Ok(false),
            Match::Whitelist(()) => return Ok(true),
            Match::None => {}
        }
        // Explicit ignore files remain active with no_ignore, matching the saved contract.
        for file in self.options.ignore_files.iter().rev() {
            let rules = self.rules(&self.root.join(file), true)?;
            match rules.matched(absolute, directory) {
                Match::Ignore(_) => return Ok(false),
                Match::Whitelist(_) => return Ok(true),
                Match::None => {}
            }
        }
        if !self.options.no_ignore {
            for parent in absolute
                .parent()
                .into_iter()
                .flat_map(Path::ancestors)
                .take_while(|parent| parent.starts_with(&self.root))
            {
                for name in IGNORE_NAMES.into_iter().rev() {
                    match self
                        .rules(&parent.join(name), false)?
                        .matched(absolute, directory)
                    {
                        Match::Ignore(_) => return Ok(false),
                        Match::Whitelist(_) => return Ok(true),
                        Match::None => {}
                    }
                }
            }
            if self.defaults.matched(absolute, directory).is_ignore() {
                return Ok(false);
            }
        }
        Ok(self.options.hidden
            || !absolute
                .file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b".")))
    }

    fn is_nested_repository(&self, directory: &Path) -> Result<bool, HostError> {
        if let Some(nested) = self
            .repositories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(directory)
        {
            return Ok(*nested);
        }
        let marker = directory.join(".git");
        let nested = match std::fs::symlink_metadata(&marker) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(HostError::storage_failure(
                    "file-selection",
                    format!(
                        "could not inspect repository marker {}: {error}",
                        marker.display()
                    ),
                ));
            }
        };
        if nested {
            // Keep one nonrecursive watch on the rejected root to detect removal of .git.
            let mut controls = self
                .control_paths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !controls.contains(&marker) {
                if controls.len() >= MAX_CACHED_IGNORE_FILES * 2 {
                    return Err(HostError::invalid_argument(
                        "too many file-selection control paths",
                    ));
                }
                controls.push(marker);
            }
        }
        let mut repositories = self
            .repositories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if repositories.len() >= MAX_CACHED_DIRECTORIES {
            repositories.clear();
        }
        repositories.insert(directory.to_path_buf(), nested);
        Ok(nested)
    }

    fn rules(&self, path: &Path, explicit: bool) -> Result<Arc<Gitignore>, HostError> {
        if let Some(value) = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .get(path)
            .cloned()
        {
            return Ok(value);
        }
        let base = if explicit {
            self.root.as_path()
        } else {
            path.parent().unwrap_or(&self.root)
        };
        let mut builder = GitignoreBuilder::new(base);
        let mut source_bytes = 0;
        let mut exists = false;
        match File::open(path) {
            Ok(file) => {
                exists = true;
                let mut contents = String::new();
                file.take(MAX_IGNORE_BYTES + 1)
                    .read_to_string(&mut contents)
                    .map_err(|error| {
                        HostError::storage_failure(
                            "file-selection",
                            format!("could not read ignore file {}: {error}", path.display()),
                        )
                    })?;
                if contents.len() as u64 > MAX_IGNORE_BYTES
                    || contents.lines().count() > MAX_IGNORE_RULES
                {
                    return Err(HostError::invalid_argument(format!(
                        "ignore file {} exceeds the rule or size limit",
                        path.display()
                    )));
                }
                source_bytes = contents.len();
                for line in contents.lines() {
                    builder
                        .add_line(Some(path.to_path_buf()), line)
                        .map_err(|error| {
                            HostError::invalid_argument(format!(
                                "invalid ignore rule in {}: {error}",
                                path.display()
                            ))
                        })?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(HostError::storage_failure(
                    "file-selection",
                    format!("could not open ignore file {}: {error}", path.display()),
                ));
            }
        }
        let value = Arc::new(builder.build().map_err(|error| {
            HostError::invalid_argument(format!("invalid ignore file {}: {error}", path.display()))
        })?);
        if exists && std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_symlink()) {
            let mut controls = self
                .control_paths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for path in std::iter::once(control_alias(path)).chain(std::fs::canonicalize(path).ok())
            {
                if !controls.contains(&path) {
                    if controls.len() >= MAX_CACHED_IGNORE_FILES * 2 {
                        return Err(HostError::invalid_argument(
                            "too many ignore-file control paths",
                        ));
                    }
                    controls.push(path);
                }
            }
        }
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.entries.len() >= MAX_CACHED_IGNORE_FILES
            || cache.source_bytes + source_bytes > MAX_CACHED_IGNORE_BYTES
        {
            *cache = IgnoreCache::default();
        }
        if cache
            .entries
            .insert(path.to_path_buf(), Arc::clone(&value))
            .is_none()
        {
            cache.source_bytes += source_bytes;
        }
        Ok(value)
    }
}

impl PathPolicy for ScanPolicy {
    fn includes_file(&self, absolute_path: &Path) -> Result<bool, HostError> {
        self.selected(absolute_path, false)
    }

    fn can_descend(&self, absolute_path: &Path) -> Result<bool, HostError> {
        if !self.selected(absolute_path, true)? {
            return Ok(false);
        }
        // Discover ignore-file symlinks even when scanning does not follow links.
        if !self.options.no_ignore {
            for name in IGNORE_NAMES {
                self.rules(&absolute_path.join(name), false)?;
            }
        }
        Ok(true)
    }

    fn control_file_changed(&self, absolute_path: &Path) -> Result<Option<PathBuf>, HostError> {
        if !self.options.nested_git
            && absolute_path.file_name().is_some_and(|name| name == ".git")
            && let Some(parent) = absolute_path.parent()
            && parent != self.root
            && let Ok(relative) = parent.strip_prefix(&self.root)
        {
            // Both creating and removing a repository boundary change index membership.
            self.invalidate()?;
            return Ok(Some(relative.to_path_buf()));
        }
        let controls = self.control_paths();
        let explicit = controls.iter().any(|file| file == absolute_path)
            || (controls
                .iter()
                .any(|file| file.file_name() == absolute_path.file_name())
                && controls.contains(&control_alias(absolute_path)));
        let scoped = absolute_path.strip_prefix(&self.root).ok();
        let local = !self.options.no_ignore
            && scoped.is_some()
            && absolute_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| IGNORE_NAMES.contains(&name));
        if !explicit && !local {
            return Ok(None);
        }
        self.invalidate()?;
        if explicit {
            return Ok(Some(PathBuf::new()));
        }
        let parent = absolute_path.parent().unwrap_or(&self.root);
        if !self.can_descend(parent)? {
            return Ok(None);
        }
        Ok(Some(
            parent
                .strip_prefix(&self.root)
                .unwrap_or(Path::new(""))
                .to_path_buf(),
        ))
    }

    fn control_paths(&self) -> Vec<PathBuf> {
        self.control_paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn invalidate(&self) -> Result<(), HostError> {
        self.repositories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        *self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = IgnoreCache::default();
        *self
            .control_paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            control_paths(&self.root, &self.options);
        Ok(())
    }
}

// Preserve the final component so replacing an ignore-file symlink stays observable.
fn control_alias(path: &Path) -> PathBuf {
    for parent in path.parent().into_iter().flat_map(Path::ancestors) {
        if let Ok(mut resolved) = std::fs::canonicalize(parent) {
            for part in path
                .strip_prefix(parent)
                .expect("ancestor prefix")
                .components()
            {
                match part {
                    std::path::Component::ParentDir => {
                        resolved.pop();
                    }
                    std::path::Component::Normal(name) => resolved.push(name),
                    _ => {}
                }
            }
            return resolved;
        }
    }
    path.to_path_buf()
}

fn control_paths(root: &Path, options: &ScanRules) -> Vec<PathBuf> {
    let mut paths = options.ignore_files.clone();
    if !options.no_ignore {
        paths.extend(IGNORE_NAMES.map(|name| root.join(name)));
    }
    let targets: Vec<_> = paths
        .iter()
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .collect();
    paths.extend(targets);
    paths.sort();
    paths.dedup();
    paths
}

fn ignore_error(error: &ignore::Error) -> EngineError {
    EngineError::invalid_argument(format!("invalid default ignore pattern: {error}"))
}
const DEFAULT_IGNORED_DIRECTORY_NAMES: [&str; 36] = [
    "node_modules",
    "vendor",
    "thirdparty",
    "third_party",
    "external",
    "deps",
    "dist",
    "build",
    "out",
    "target",
    "coverage",
    "generated",
    "__pycache__",
    "venv",
    ".venv",
    "env",
    ".tox",
    ".eggs",
    "Pods",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".vite",
    ".parcel-cache",
    ".cache",
    ".gradle",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "tmp",
    "temp",
    "logs",
    "locale",
    "locales",
    "translations",
];

const DEFAULT_IGNORED_FILE_PATTERNS: [&str; 23] = [
    "*.lock",
    "*.lockb",
    "*-lock.json",
    "*-lock.yaml",
    "npm-shrinkwrap.json",
    "go.sum",
    "*.resolved",
    "*.po",
    "*.pot",
    "*.map",
    "*.min.*",
    "*.bundle.*",
    "*.generated.*",
    "*.gen.*",
    "*.designer.*",
    "*.pb.*",
    "*_pb2.*",
    "*.g.*",
    "*.gif",
    "*.jpeg",
    "*.jpg",
    "*.png",
    "*.webp",
];
