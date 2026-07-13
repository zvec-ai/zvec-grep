use std::{
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;

use crate::{EngineError, FileFormat, FileId, Result, file::ScannedFile};

const DEFAULT_MAX_FILE_SIZE: u64 = 1024 * 1024;
const DEFAULT_SKIPPED_DIRECTORIES: &[&str] = &[
    ".git",
    ".zvec-grep",
    ".venv",
    "__pycache__",
    "build",
    "dist",
    "node_modules",
    "target",
    "vendor",
];

#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_hidden: bool,
    pub max_file_size: u64,
}

impl WorkspaceConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            include: Vec::new(),
            exclude: Vec::new(),
            include_hidden: false,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }
}

#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
    include: Option<GlobSet>,
    exclude: GlobSet,
    include_hidden: bool,
    max_file_size: u64,
}

impl Workspace {
    pub fn open(config: WorkspaceConfig) -> Result<Self> {
        let root = fs::canonicalize(&config.root).map_err(|source| EngineError::Io {
            path: config.root.clone(),
            source,
        })?;
        if !root.is_dir() {
            return Err(EngineError::InvalidConfig(format!(
                "workspace root is not a directory: {}",
                root.display()
            )));
        }

        Ok(Self {
            root,
            include: if config.include.is_empty() {
                None
            } else {
                Some(build_globs(&config.include)?)
            },
            exclude: build_globs(&config.exclude)?,
            include_hidden: config.include_hidden,
            max_file_size: config.max_file_size,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn scan(&self) -> Result<Vec<ScannedFile>> {
        let mut builder = WalkBuilder::new(&self.root);
        builder
            .hidden(!self.include_hidden)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .ignore(true)
            .parents(true)
            .follow_links(false);
        builder.filter_entry(|entry| {
            !entry.file_type().is_some_and(|kind| kind.is_dir())
                || !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| DEFAULT_SKIPPED_DIRECTORIES.contains(&name))
        });

        let mut files = Vec::new();
        for entry in builder.build() {
            let entry = entry.map_err(|error| EngineError::InvalidConfig(error.to_string()))?;
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }

            let absolute_path = entry.into_path();
            let relative_path = absolute_path
                .strip_prefix(&self.root)
                .expect("walked paths remain under the workspace root")
                .to_path_buf();
            if relative_path.starts_with(".zvec-grep")
                || self.exclude.is_match(&relative_path)
                || self
                    .include
                    .as_ref()
                    .is_some_and(|set| !set.is_match(&relative_path))
            {
                continue;
            }

            let metadata = fs::metadata(&absolute_path).map_err(|source| EngineError::Io {
                path: absolute_path.clone(),
                source,
            })?;
            if metadata.len() > self.max_file_size {
                continue;
            }

            files.push(ScannedFile {
                id: FileId::from_relative_path(&relative_path),
                absolute_path,
                relative_path: relative_path.clone(),
                format: FileFormat::detect(&relative_path),
                size: metadata.len(),
                modified_ms: metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |duration| duration.as_millis()),
            });
        }
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(files)
    }
}

fn build_globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).map_err(|error| {
            EngineError::InvalidConfig(format!("invalid glob {pattern:?}: {error}"))
        })?);
    }
    builder
        .build()
        .map_err(|error| EngineError::InvalidConfig(format!("invalid glob set: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_respects_gitignore_and_filters() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::create_dir_all(temp.path().join("target")).unwrap();
        fs::write(temp.path().join(".gitignore"), "target/\n").unwrap();
        fs::write(temp.path().join("src/lib.rs"), "fn main() {}\n").unwrap();
        fs::write(temp.path().join("src/lib.test.rs"), "ignored\n").unwrap();
        fs::write(temp.path().join("target/out.rs"), "ignored\n").unwrap();

        let mut config = WorkspaceConfig::new(temp.path());
        config.include = vec!["src/**".into()];
        config.exclude = vec!["**/*.test.rs".into()];
        let workspace = Workspace::open(config).unwrap();
        let files = workspace.scan().unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative_path, Path::new("src/lib.rs"));
    }

    #[test]
    fn scan_skips_large_files() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("large.txt"), "too large").unwrap();
        let mut config = WorkspaceConfig::new(temp.path());
        config.max_file_size = 3;
        assert!(Workspace::open(config).unwrap().scan().unwrap().is_empty());
    }

    #[test]
    fn scan_does_not_descend_into_dependencies_or_build_output() {
        let temp = tempfile::tempdir().unwrap();
        for directory in ["node_modules", "target", "vendor"] {
            fs::create_dir(temp.path().join(directory)).unwrap();
            fs::write(temp.path().join(directory).join("dependency.rs"), "ignored").unwrap();
        }
        fs::write(temp.path().join("kept.rs"), "kept").unwrap();
        let files = Workspace::open(WorkspaceConfig::new(temp.path()))
            .unwrap()
            .scan()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative_path, Path::new("kept.rs"));
    }
}
