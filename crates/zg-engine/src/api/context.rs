//! Types used by [`crate::ZvecGrep::context`].

pub use options::ContextOptions;
pub use result::ContextResult;

/// Options accepted by [`crate::ZvecGrep::context`].
pub mod options {
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[allow(clippy::struct_excessive_bools)]
    pub struct ContextOptions {
        pub query: Option<String>,
        pub queries: Vec<String>,
        pub rg: bool,
        pub rg_options: RgOptions,
        pub rg_paths: Vec<PathBuf>,
        pub routes: Vec<ContextRoute>,
        pub fuse: bool,
        /// Workspace root. `None` uses the process working directory.
        pub root: Option<PathBuf>,
        pub limit: Option<usize>,
        pub auto_update: bool,
        /// Explicit refresh policy; absent preserves the legacy auto-update behavior.
        #[serde(default)]
        pub refresh: Option<RefreshPolicy>,
        pub trace: bool,
        pub prefer_symbol: bool,
        pub symbol_types: Vec<SymbolType>,
        pub include_paths: Vec<String>,
        pub exclude_paths: Vec<String>,
        pub globs: Vec<String>,
        pub insensitive_globs: Vec<String>,
        pub file_types: Vec<String>,
        pub excluded_file_types: Vec<String>,
        pub hidden: bool,
        pub no_ignore: bool,
        pub ignore_files: Vec<PathBuf>,
        pub max_depth: Option<usize>,
        pub max_file_size_bytes: Option<u64>,
        pub follow: bool,
        pub modified_after_epoch_ms: Option<u64>,
        pub modified_before_epoch_ms: Option<u64>,
        pub embedding_concurrency: Option<usize>,
        /// Allows remote embedding for this operation without persisting a grant.
        #[serde(default)]
        pub allow_remote: bool,
        #[serde(default)]
        pub api_key: Option<String>,
        #[serde(default)]
        pub endpoint: Option<String>,
        /// Model disclosed by an interactive caller; reject a changed index model.
        #[serde(default)]
        pub authorization_model: Option<String>,
        #[serde(default)]
        pub device: Option<crate::api::index::options::Device>,
        #[serde(default)]
        pub model_cache: Option<PathBuf>,
    }

    impl Default for ContextOptions {
        fn default() -> Self {
            Self {
                query: None,
                queries: Vec::new(),
                rg: false,
                rg_options: RgOptions::default(),
                rg_paths: Vec::new(),
                routes: Vec::new(),
                fuse: false,
                root: None,
                limit: None,
                auto_update: true,
                refresh: None,
                trace: false,
                prefer_symbol: false,
                symbol_types: Vec::new(),
                include_paths: Vec::new(),
                exclude_paths: Vec::new(),
                globs: Vec::new(),
                insensitive_globs: Vec::new(),
                file_types: Vec::new(),
                excluded_file_types: Vec::new(),
                hidden: false,
                no_ignore: false,
                ignore_files: Vec::new(),
                max_depth: None,
                max_file_size_bytes: None,
                follow: false,
                modified_after_epoch_ms: None,
                modified_before_epoch_ms: None,
                embedding_concurrency: None,
                allow_remote: false,
                api_key: None,
                endpoint: None,
                authorization_model: None,
                device: None,
                model_cache: None,
            }
        }
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RefreshPolicy {
        Background,
        Wait,
        Off,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct ContextRoute {
        pub mode: ContextRouteMode,
        pub query: String,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextRouteMode {
        Fts,
        Vector,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub struct RgOptions {
        pub extra_args: Vec<String>,
        pub pattern_files: Vec<PathBuf>,
        pub fixed_strings: bool,
        pub ignore_case: bool,
        pub word_regexp: bool,
        pub before_context: usize,
        pub after_context: usize,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SymbolType {
        Module,
        Class,
        Interface,
        Function,
        Value,
        Alias,
    }
}

/// Values returned by [`crate::ZvecGrep::context`].
pub mod result {
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    use super::options::SymbolType;

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    pub struct ContextResult {
        pub query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub freshness: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub background_refresh: Option<String>,
        pub root: PathBuf,
        pub source: ContextSource,
        pub coverage: ContextCoverage,
        pub workspace_index: Option<ContextWorkspaceIndex>,
        pub items: Vec<ContextItem>,
        pub group_results: Vec<ContextGroupResult>,
        pub diagnostics: ContextDiagnostics,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    pub struct ContextGroupResult {
        pub id: String,
        pub query: String,
        pub role: ContextQueryGroupRole,
        pub items: Vec<ContextItem>,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextSource {
        Index,
        Rg,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextCoverage {
        RankedSample,
        RgExhaustive,
        RgTruncated,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct ContextWorkspaceIndex {
        pub id: String,
        pub name: String,
        pub path: PathBuf,
        pub generation: Option<u64>,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    pub struct ContextItem {
        pub kind: ContextItemKind,
        pub rank: usize,
        pub absolute_path: PathBuf,
        pub relative_path: PathBuf,
        pub range: ContentRange,
        pub excerpt_range: Option<ContentRange>,
        pub content: String,
        pub content_role: Option<ContextContentRole>,
        pub outline: Option<String>,
        pub status: ContextItemStatus,
        pub score: Option<f64>,
        pub matched_by: MatchedBy,
        pub metadata: Option<EntityMetadata>,
        pub entity_id: Option<String>,
        pub container: Option<ContextContainer>,
        pub trace: Option<SearchHitTrace>,
        pub query_groups: Vec<ContextQueryGroupMatch>,
        pub selection_reason: Option<ContextSelectionReason>,
        pub coverage_group: Option<String>,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextContentRole {
        Source,
        Outline,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct ContextContainer {
        pub entity_id: String,
        pub range: ContentRange,
        pub metadata: Option<EntityMetadata>,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    pub struct ContextQueryGroupMatch {
        pub id: String,
        pub query: String,
        pub role: ContextQueryGroupRole,
        pub rank: usize,
        pub matched_by: MatchedBy,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextQueryGroupRole {
        Primary,
        Supplemental,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextSelectionReason {
        Coverage,
        GlobalFill,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextItemKind {
        IndexedEntity,
        LexicalMatch,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextItemStatus {
        Fresh,
        PossiblyStale,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum MatchedBy {
        Fts,
        Vector,
        #[serde(rename = "fts+vector")]
        FtsAndVector,
        Lexical,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub struct ContextDiagnostics {
        pub empty_reason: Option<EmptyReason>,
        pub index: Option<IndexDiagnostics>,
        pub rg: Option<RgDiagnostics>,
        pub structure: Option<StructureEnrichmentDiagnostics>,
        pub timings: Vec<TimingEntry>,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct IndexDiagnostics {
        pub hits_returned: usize,
        pub query_groups: Vec<IndexQueryGroupDiagnostics>,
        pub routes: Vec<IndexRouteDiagnostics>,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct IndexQueryGroupDiagnostics {
        pub id: String,
        pub query: String,
        pub role: ContextQueryGroupRole,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct IndexRouteDiagnostics {
        pub id: String,
        pub mode: super::options::ContextRouteMode,
        pub query: String,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct StructureEnrichmentDiagnostics {
        pub source: StructureEnrichmentSource,
        pub file_limit: usize,
        pub matched_files: usize,
        pub parsed_files: usize,
        pub enriched_files: usize,
        pub enriched_items: usize,
        pub skipped_files: usize,
        pub truncated: bool,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum StructureEnrichmentSource {
        StructuralExtraction,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    pub struct SearchHitTrace {
        pub recall: Vec<SearchRecallTrace>,
        pub fusion: SearchFusionTrace,
        pub final_selection: SearchFinalTrace,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    pub struct SearchRecallTrace {
        pub path: super::options::ContextRouteMode,
        pub route_id: String,
        pub query: String,
        pub found: bool,
        pub rank: Option<usize>,
        pub score: Option<f64>,
        pub forced: bool,
        pub reason: Option<String>,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    pub struct SearchFusionTrace {
        pub rank: usize,
        pub score: f64,
        pub forced: bool,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct SearchFinalTrace {
        pub returned_by_limit: bool,
        pub cutoff_rank: usize,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum EmptyReason {
        NoMatches,
        NoSearchableFiles,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct RgDiagnostics {
        pub backend: String,
        pub command: PathBuf,
        pub args: Vec<String>,
        pub ignored_directories: Vec<PathBuf>,
        pub missing_paths: Vec<PathBuf>,
        pub searched_paths: Vec<PathBuf>,
        pub limit: Option<usize>,
        pub truncated: bool,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct TimingEntry {
        pub name: String,
        pub duration_micros: u64,
        pub count: Option<u64>,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case", tag = "kind")]
    pub enum ContentRange {
        File,
        /// UTF-16 offsets are source-global for extracted entities and line-local
        /// columns for lexical match spans.
        Text {
            start_line: usize,
            end_line: usize,
            start_offset: usize,
            end_offset: usize,
        },
        Byte {
            start_offset: u64,
            end_offset: u64,
        },
        Page {
            page: usize,
        },
        PageText {
            page: usize,
            start_offset: usize,
            end_offset: usize,
        },
        PageRegion {
            page: usize,
            x: u32,
            y: u32,
            width: u32,
            height: u32,
        },
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case", tag = "kind")]
    pub enum EntityMetadata {
        Code {
            symbol_type: SymbolType,
            symbol_name: Option<String>,
            scope: Option<String>,
            node_type: Option<String>,
            signature: Option<String>,
            documentation: Option<String>,
            modifiers: Vec<String>,
        },
        Markdown {
            heading: Option<String>,
            level: Option<usize>,
            scope: Option<String>,
        },
    }
}

impl From<crate::domain::SymbolType> for options::SymbolType {
    fn from(value: crate::domain::SymbolType) -> Self {
        match value {
            crate::domain::SymbolType::Module => Self::Module,
            crate::domain::SymbolType::Class => Self::Class,
            crate::domain::SymbolType::Interface => Self::Interface,
            crate::domain::SymbolType::Function => Self::Function,
            crate::domain::SymbolType::Value => Self::Value,
            crate::domain::SymbolType::Alias => Self::Alias,
        }
    }
}

impl From<options::SymbolType> for crate::domain::SymbolType {
    fn from(value: options::SymbolType) -> Self {
        match value {
            options::SymbolType::Module => Self::Module,
            options::SymbolType::Class => Self::Class,
            options::SymbolType::Interface => Self::Interface,
            options::SymbolType::Function => Self::Function,
            options::SymbolType::Value => Self::Value,
            options::SymbolType::Alias => Self::Alias,
        }
    }
}

impl From<crate::domain::SourceRange> for result::ContentRange {
    fn from(range: crate::domain::SourceRange) -> Self {
        use crate::domain::SourceRange;
        match range {
            SourceRange::File => Self::File,
            SourceRange::Text(range) => Self::Text {
                start_line: range.start_line,
                end_line: range.end_line,
                start_offset: range.start_utf16_offset,
                end_offset: range.end_utf16_offset,
            },
            SourceRange::Byte {
                start_offset,
                end_offset,
            } => Self::Byte {
                start_offset,
                end_offset,
            },
            SourceRange::Page { page } => Self::Page { page },
            SourceRange::PageText {
                page,
                start_utf16_offset,
                end_utf16_offset,
            } => Self::PageText {
                page,
                start_offset: start_utf16_offset,
                end_offset: end_utf16_offset,
            },
            SourceRange::PageRegion {
                page,
                x,
                y,
                width,
                height,
            } => Self::PageRegion {
                page,
                x,
                y,
                width,
                height,
            },
        }
    }
}

impl From<&crate::domain::SourceRange> for result::ContentRange {
    fn from(range: &crate::domain::SourceRange) -> Self {
        (*range).into()
    }
}

impl From<crate::domain::LineColumnRange> for result::ContentRange {
    fn from(range: crate::domain::LineColumnRange) -> Self {
        Self::Text {
            start_line: range.start.line,
            end_line: range.end.line,
            start_offset: range.start.column_utf16,
            end_offset: range.end.column_utf16,
        }
    }
}

impl From<&crate::domain::LineColumnRange> for result::ContentRange {
    fn from(range: &crate::domain::LineColumnRange) -> Self {
        (*range).into()
    }
}

impl From<crate::domain::EntityMetadata> for result::EntityMetadata {
    fn from(metadata: crate::domain::EntityMetadata) -> Self {
        match metadata {
            crate::domain::EntityMetadata::Code {
                symbol_type,
                symbol_name,
                scope,
                node_type,
                signature,
                documentation,
                modifiers,
            } => Self::Code {
                symbol_type: symbol_type.into(),
                symbol_name,
                scope,
                node_type,
                signature,
                documentation,
                modifiers,
            },
            crate::domain::EntityMetadata::Markdown {
                heading,
                level,
                scope,
            } => Self::Markdown {
                heading,
                level,
                scope,
            },
        }
    }
}

impl From<&crate::domain::EntityMetadata> for result::EntityMetadata {
    fn from(metadata: &crate::domain::EntityMetadata) -> Self {
        metadata.clone().into()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::domain::{
        EntityMetadata, LineColumnRange, SourceRange, SymbolType, TextPosition, TextRange,
    };

    use super::result;

    #[test]
    fn domain_values_preserve_public_wire_coordinates_and_metadata() {
        let indexed: result::ContentRange = SourceRange::Text(TextRange {
            start_line: 2,
            end_line: 3,
            start_utf16_offset: 12,
            end_utf16_offset: 24,
        })
        .into();
        assert_eq!(
            serde_json::to_value(indexed).expect("indexed range"),
            json!({
                "kind": "text", "start_line": 2, "end_line": 3, "start_offset": 12, "end_offset": 24,
            })
        );
        let lexical: result::ContentRange = LineColumnRange {
            start: TextPosition {
                line: 2,
                column_utf16: 8,
            },
            end: TextPosition {
                line: 3,
                column_utf16: 2,
            },
        }
        .into();
        assert_eq!(
            serde_json::to_value(lexical).expect("lexical range"),
            json!({
                "kind": "text", "start_line": 2, "end_line": 3, "start_offset": 8, "end_offset": 2,
            })
        );
        let metadata: result::EntityMetadata = EntityMetadata::Code {
            symbol_type: SymbolType::Function,
            symbol_name: Some("calculate".to_owned()),
            scope: None,
            node_type: None,
            signature: None,
            documentation: None,
            modifiers: vec!["public".to_owned()],
        }
        .into();
        assert_eq!(
            serde_json::to_value(metadata).expect("metadata"),
            json!({
                "kind": "code", "symbol_type": "function", "symbol_name": "calculate", "scope": null,
                "node_type": null, "signature": null, "documentation": null, "modifiers": ["public"],
            })
        );
    }
}
