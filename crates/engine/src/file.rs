use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Stable identity of a file inside a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileId(String);

impl FileId {
    pub fn from_relative_path(path: &Path) -> Self {
        Self(
            blake3::hash(path.to_string_lossy().as_bytes())
                .to_hex()
                .to_string(),
        )
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_stored(value: String) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileFormat {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Go,
    Java,
    C,
    Cpp,
    Markdown,
    Text,
    Json,
    Toml,
    Yaml,
    Pdf,
    Image,
    Binary,
}

impl FileFormat {
    pub fn detect(path: &Path) -> Self {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if matches!(name, "Dockerfile" | "Makefile") {
            return Self::Text;
        }

        match path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "rs" => Self::Rust,
            "ts" | "tsx" => Self::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "py" => Self::Python,
            "go" => Self::Go,
            "java" => Self::Java,
            "c" => Self::C,
            "cc" | "cpp" | "cxx" | "h" | "hpp" => Self::Cpp,
            "md" | "mdx" => Self::Markdown,
            "json" | "jsonc" => Self::Json,
            "toml" => Self::Toml,
            "yaml" | "yml" => Self::Yaml,
            "pdf" => Self::Pdf,
            "png" | "jpg" | "jpeg" | "gif" | "webp" => Self::Image,
            "zip" | "gz" | "7z" | "exe" | "dll" | "so" | "dylib" | "wasm" => Self::Binary,
            _ => Self::Text,
        }
    }

    pub fn is_text(self) -> bool {
        !matches!(self, Self::Pdf | Self::Image | Self::Binary)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileLocation {
    Lines { start: usize, end: usize },
    Page { number: usize },
    WholeFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentKind {
    Code,
    Section,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodeSymbolKind {
    Function,
    Method,
    Type,
    Interface,
    Enum,
    Trait,
    Module,
    Constant,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeSymbol {
    pub name: String,
    pub kind: CodeSymbolKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSegment {
    pub id: String,
    pub file_id: FileId,
    pub relative_path: PathBuf,
    pub kind: SegmentKind,
    pub symbol: Option<CodeSymbol>,
    pub location: FileLocation,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScannedFile {
    pub id: FileId,
    pub absolute_path: PathBuf,
    pub relative_path: PathBuf,
    pub format: FileFormat,
    pub size: u64,
    pub modified_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedFile {
    pub id: FileId,
    pub relative_path: PathBuf,
    pub format: FileFormat,
    pub size: u64,
    pub modified_ms: u128,
    pub content_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_formats() {
        assert_eq!(
            FileFormat::detect(Path::new("src/lib.rs")),
            FileFormat::Rust
        );
        assert_eq!(
            FileFormat::detect(Path::new("README.md")),
            FileFormat::Markdown
        );
        assert_eq!(FileFormat::detect(Path::new("manual.pdf")), FileFormat::Pdf);
        assert_eq!(
            FileFormat::detect(Path::new("archive.zip")),
            FileFormat::Binary
        );
    }

    #[test]
    fn file_ids_are_stable_for_the_same_relative_path() {
        let first = FileId::from_relative_path(Path::new("src/lib.rs"));
        let second = FileId::from_relative_path(Path::new("src/lib.rs"));
        assert_eq!(first, second);
    }
}
