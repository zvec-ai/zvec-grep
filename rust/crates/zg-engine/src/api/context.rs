//! Types used by [`crate::ZvecGrep::context`].

pub use options::ContextOptions;
pub use result::ContextResult;

/// Options accepted by [`crate::ZvecGrep::context`].
pub mod options {
    pub use crate::domain::{FileCategory, FileFormat, GlobRule, SymbolType};

    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

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

    /// Temporary restrictions on indexed files and entities for one query.
    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct QueryFilter {
        /// Ordered path rules relative to the workspace root.
        pub globs: Vec<GlobRule>,
        /// Formats matched by the catalog's explicit, case-sensitive filename rules.
        pub formats: Vec<FileFormat>,
        /// Excluded file-name formats; any match takes precedence over included formats.
        pub excluded_formats: Vec<FileFormat>,
        /// Categories selected through the catalog's filename rules.
        pub categories: Vec<FileCategory>,
        /// Excluded file-name categories; any match takes precedence over included categories.
        pub excluded_categories: Vec<FileCategory>,
        /// Inclusive bounds on the modification time recorded in the index.
        pub modified_after_epoch_ms: Option<u64>,
        pub modified_before_epoch_ms: Option<u64>,
        pub symbol_types: Vec<SymbolType>,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(deny_unknown_fields)]
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
        /// Temporary filter over indexed files; does not alter workspace selection.
        #[serde(default)]
        pub filter: QueryFilter,
        /// Filesystem glob options for the independent rg backend only.
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
        pub embedding_concurrency: Option<usize>,
        /// Allows remote embedding for this operation without persisting a grant.
        #[serde(default)]
        pub allow_remote: bool,
        /// Exact destinations approved by interactive consent for this operation.
        /// Separate from the explicit blanket `allow_remote` option.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub authorized_remote: Vec<crate::authorization::IndexAuthorization>,
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
        /// Runtime-only progress for synchronous refreshes.
        #[serde(skip)]
        pub on_progress: Option<crate::api::index::progress::IndexProgressReporter>,
        /// Runtime-only cooperative cancellation for this request.
        #[serde(skip)]
        pub signal: Option<tokio_util::sync::CancellationToken>,
    }

    impl ContextOptions {
        pub(crate) fn validate_file_selection(&self) -> crate::EngineResult<()> {
            if self.rg {
                if self.filter != QueryFilter::default() {
                    return Err(crate::EngineError::invalid_argument(
                        "indexed file filters cannot be combined with rg; use rg glob and type options",
                    ));
                }
            } else if !self.globs.is_empty()
                || !self.insensitive_globs.is_empty()
                || !self.file_types.is_empty()
                || !self.excluded_file_types.is_empty()
                || self.hidden
                || self.no_ignore
                || self.follow
                || !self.ignore_files.is_empty()
                || self.max_depth.is_some()
                || self.max_file_size_bytes.is_some()
                || !self.rg_paths.is_empty()
                || self.rg_options != RgOptions::default()
            {
                return Err(crate::EngineError::invalid_argument(
                    "rg filesystem options require rg mode; use filter for indexed queries and index to change scanning settings",
                ));
            }
            Ok(())
        }
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
                filter: QueryFilter::default(),
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
                embedding_concurrency: None,
                allow_remote: false,
                authorized_remote: Vec::new(),
                api_key: None,
                endpoint: None,
                authorization_model: None,
                device: None,
                model_cache: None,
                on_progress: None,
                signal: None,
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

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(default)]
    #[allow(clippy::struct_excessive_bools)]
    pub struct RgOptions {
        pub extra_args: Vec<String>,
        pub pattern_files: Vec<PathBuf>,
        /// Inclusive bounds on the file modification time read during direct search.
        pub modified_after_epoch_ms: Option<u64>,
        pub modified_before_epoch_ms: Option<u64>,
        pub fixed_strings: bool,
        pub ignore_case: bool,
        pub word_regexp: bool,
        pub before_context: usize,
        pub after_context: usize,
        pub smart_case: bool,
        pub line_regexp: bool,
        pub invert_match: bool,
        pub multiline: bool,
        pub multiline_dotall: bool,
        pub crlf: bool,
        pub text: bool,
        pub no_unicode: bool,
        pub stop_on_nonmatch: bool,
        pub max_count: Option<usize>,
        pub threads: Option<usize>,
        pub regex_size_limit: Option<usize>,
        pub dfa_size_limit: Option<usize>,
        pub no_ignore_dot: bool,
        pub no_ignore_files: bool,
        pub no_ignore_global: bool,
        pub no_ignore_parent: bool,
        pub no_ignore_vcs: bool,
        pub one_file_system: bool,
        pub glob_case_insensitive: bool,
        /// CLI glob rules retain their order across -g and --iglob.
        pub glob_rules: Vec<RgGlob>,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct RgGlob {
        pub pattern: String,
        pub case_insensitive: bool,
    }

    #[cfg(test)]
    mod selection_tests {
        use super::*;

        #[test]
        fn query_modes_reject_each_others_selection_contract() {
            let indexed = ContextOptions {
                hidden: true,
                ..Default::default()
            };
            assert!(indexed.validate_file_selection().is_err());
            let rg = ContextOptions {
                rg: true,
                filter: QueryFilter {
                    globs: vec!["*.rs".into()],
                    ..Default::default()
                },
                ..Default::default()
            };
            assert!(rg.validate_file_selection().is_err());
            let indexed = ContextOptions {
                filter: rg.filter,
                ..Default::default()
            };
            assert!(indexed.validate_file_selection().is_ok());

            let rg_time_filter = RgOptions {
                modified_after_epoch_ms: Some(0),
                ..Default::default()
            };
            let indexed = ContextOptions {
                rg_options: rg_time_filter.clone(),
                ..Default::default()
            };
            assert!(indexed.validate_file_selection().is_err());
            let rg = ContextOptions {
                rg: true,
                rg_options: rg_time_filter,
                ..Default::default()
            };
            assert!(rg.validate_file_selection().is_ok());
        }

        #[test]
        fn query_filter_accepts_partial_fields_and_rejects_nested_or_scan_fields() {
            let filter: QueryFilter =
                serde_json::from_str(r#"{"formats":["rust"],"modified_after_epoch_ms":0}"#)
                    .expect("partial query filter");
            assert_eq!(filter.formats, vec![FileFormat::Rust]);
            assert_eq!(filter.modified_after_epoch_ms, Some(0));
            assert!(filter.globs.is_empty());
            for value in [r#"{"files":{}}"#, r#"{"hidden":true}"#] {
                assert!(serde_json::from_str::<QueryFilter>(value).is_err());
            }
        }
    }
}

/// Values returned by [`crate::ZvecGrep::context`].
pub mod result {
    pub use crate::domain::{CodeMetadata, EntityMetadata, MarkdownMetadata};

    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum MatchedBy {
        Fts,
        Vector,
        #[serde(rename = "fts+vector")]
        FtsAndVector,
        Lexical,
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

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct TimingEntry {
        pub name: String,
        pub duration_micros: u64,
        pub count: Option<u64>,
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
        pub name: String,
        pub path: PathBuf,
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
    #[serde(rename_all = "snake_case", tag = "kind")]
    pub enum ContentRange {
        File,
        /// Half-open UTF-8 byte offsets and columns; endpoint lines are one-based.
        Text {
            start_line: usize,
            end_line: usize,
            start_byte_offset: usize,
            end_byte_offset: usize,
            start_byte_column: usize,
            end_byte_column: usize,
        },
        Byte {
            start_offset: u64,
            end_offset: u64,
        },
    }
}

impl From<crate::domain::Range> for result::ContentRange {
    fn from(range: crate::domain::Range) -> Self {
        use crate::domain::Range;
        match range {
            Range::Full => Self::File,
            Range::Text(range) => Self::Text {
                start_line: range.start_line(),
                end_line: range.end_line(),
                start_byte_offset: range.start_byte_offset(),
                end_byte_offset: range.end_byte_offset(),
                start_byte_column: range.start_byte_column(),
                end_byte_column: range.end_byte_column(),
            },
            Range::Byte(range) => Self::Byte {
                start_offset: range.start_offset,
                end_offset: range.end_offset,
            },
        }
    }
}

impl From<&crate::domain::Range> for result::ContentRange {
    fn from(range: &crate::domain::Range) -> Self {
        (*range).into()
    }
}

impl From<crate::domain::TextRange> for result::ContentRange {
    fn from(range: crate::domain::TextRange) -> Self {
        crate::domain::Range::Text(range).into()
    }
}

impl From<&crate::domain::TextRange> for result::ContentRange {
    fn from(range: &crate::domain::TextRange) -> Self {
        (*range).into()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::domain::{ByteRange, Range, TextRange};

    use super::result;

    #[test]
    fn domain_ranges_preserve_public_wire_coordinates() {
        let file: result::ContentRange = Range::Full.into();
        assert_eq!(
            serde_json::to_value(file).expect("file range"),
            json!({ "kind": "file" })
        );
        let bytes: result::ContentRange = Range::Byte(ByteRange {
            start_offset: 12,
            end_offset: 24,
        })
        .into();
        assert_eq!(
            serde_json::to_value(bytes).expect("byte range"),
            json!({ "kind": "byte", "start_offset": 12, "end_offset": 24 })
        );
        let indexed: result::ContentRange = TextRange::from_coordinates(6, 13, 2, 3, 0, 0)
            .expect("indexed range")
            .into();
        assert_eq!(
            serde_json::to_value(indexed).expect("indexed range"),
            json!({
                "kind": "text", "start_line": 2, "end_line": 3,
                "start_byte_offset": 6, "end_byte_offset": 13,
                "start_byte_column": 0, "end_byte_column": 0,
            })
        );
        let lexical: result::ContentRange = TextRange::from_coordinates(9, 12, 2, 2, 3, 6)
            .expect("lexical range")
            .into();
        assert_eq!(
            serde_json::to_value(lexical).expect("lexical range"),
            json!({
                "kind": "text", "start_line": 2, "end_line": 2,
                "start_byte_offset": 9, "end_byte_offset": 12,
                "start_byte_column": 3, "end_byte_column": 6,
            })
        );
    }
}
