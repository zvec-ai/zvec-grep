//! Symbol-based queries over persisted directed relationships.

use std::path::{Path, PathBuf};

pub use crate::domain::{Direction, GlobRule, GraphEdgeKind};

use serde::{Deserialize, Serialize};

/// Maximum number of exact definitions in a relationship query.
pub const MAX_SYMBOL_MATCHES: usize = 10;
/// Maximum returned edges per matched definition.
pub const MAX_RELATIONSHIP_EDGES: usize = 50;
/// Maximum length of a relationship symbol query in characters.
pub const MAX_RELATIONSHIP_SYMBOL_CHARS: usize = 1024;

/// Reads an existing workspace index without loading models or refreshing it.
/// Requires a workspace indexed with [`crate::api::index::IndexOptions::graph_indexing`].
/// Each matched definition and its edges are read from one consistent store snapshot.
/// Different matches may reflect different indexing moments; roots changed since selection are skipped.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelationshipOptions {
    /// Workspace location. `None` uses the working directory.
    pub root: Option<PathBuf>,
    /// Symbol name, optionally qualified as `scope::name` or `scope.name`.
    /// `::name` selects only top-level definitions; plain `name` matches any scope.
    /// Literal symbol names take precedence over dotted qualification.
    /// A trailing dot is part of the literal name.
    /// Absolute qualified paths such as `::ns::name` are equivalent to `ns::name`.
    /// Missing or unrepresentable exact definitions may return similarly named suggestions without edges.
    /// Queries exceeding [`MAX_SYMBOL_MATCHES`] indexed definitions fail; narrow the scope or use `globs`.
    pub symbol: String,
    /// Edge types to include. `None` selects all types; an empty list selects none.
    /// `callers` and `callees` always override this with `Calls`.
    pub kinds: Option<Vec<GraphEdgeKind>>,
    /// Ordered workspace-relative glob rules, using the same semantics as indexed search.
    /// Filter definitions and suggestions; edge source files remain unrestricted.
    pub globs: Vec<GlobRule>,
    /// Maximum returned edges per matched symbol; defaults to 20, capped at [`MAX_RELATIONSHIP_EDGES`].
    pub limit: Option<usize>,
    /// Maximum workspace lock wait in milliseconds; defaults to 30 seconds.
    pub lock_timeout_ms: Option<u64>,
    /// Runtime-only cooperative cancellation.
    #[serde(skip)]
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

impl RelationshipOptions {
    /// Validate query parameters before opening storage or contacting a daemon.
    ///
    /// # Errors
    /// Returns an invalid-argument error for an empty/overlong symbol, invalid glob, or out-of-range limit.
    pub fn validate(&self) -> crate::EngineResult<()> {
        let symbol = self.symbol.trim();
        if symbol.is_empty() || symbol.chars().count() > MAX_RELATIONSHIP_SYMBOL_CHARS {
            return Err(crate::EngineError::invalid_argument(format!(
                "symbol must contain between 1 and {MAX_RELATIONSHIP_SYMBOL_CHARS} characters",
            )));
        }
        if symbol.ends_with("::") {
            return Err(crate::EngineError::invalid_argument(
                "expected a symbol name after '::', e.g. 'Foo::bar'",
            ));
        }
        crate::file_selection::GlobMatcher::new(Path::new(""), &self.globs)?;
        if self.limit == Some(0) {
            return Err(crate::EngineError::invalid_argument(
                "relationship limit must be positive",
            ));
        }
        if self
            .limit
            .is_some_and(|limit| limit > MAX_RELATIONSHIP_EDGES)
        {
            return Err(crate::EngineError::invalid_argument(format!(
                "relationship limit must not exceed {MAX_RELATIONSHIP_EDGES}",
            )));
        }
        Ok(())
    }
}

/// A definition in workspace-relative source coordinates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDefinition {
    pub name: String,
    #[serde(with = "native_path")]
    pub file_path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
}

/// One resolved edge. `symbol` describes the opposite endpoint when available.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationshipEdge {
    pub kind: GraphEdgeKind,
    /// Direction relative to the matched definition; self-loops are `Both`.
    pub direction: Direction,
    /// Workspace-relative source endpoint file, even without symbol metadata.
    #[serde(with = "native_path")]
    pub file_path: PathBuf,
    pub symbol: Option<SymbolDefinition>,
    /// One-based source location of the relationship, when available.
    pub line: Option<usize>,
    /// Zero-based byte column.
    pub column: Option<usize>,
}

/// One matched definition and its resolved edges, preserving distinct source locations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolRelationships {
    #[serde(flatten)]
    pub symbol: SymbolDefinition,
    /// Whether additional visible edges were omitted by the per-symbol limit.
    pub truncated: bool,
    pub edges: Vec<RelationshipEdge>,
}

/// Exact relationship matches, or suggestions to retry when no definition matched.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationshipResult {
    pub matches: Vec<SymbolRelationships>,
    /// No usable exact definition matched; these similarly named definitions carry no edges.
    /// At most five best-effort suggestions, not an exhaustive list of similar symbols.
    pub suggestions: Vec<SymbolDefinition>,
}

// Keep ordinary JSON paths as strings. Serde's platform-tagged OsString encoding
// preserves non-Unicode paths across daemon replies until the text renderer runs.
mod native_path {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::{
        ffi::OsString,
        path::{Path, PathBuf},
    };

    pub(super) fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        match path.to_str() {
            Some(text) => text.serialize(serializer),
            None => path.as_os_str().serialize(serializer),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<PathBuf, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum EncodedPath {
            Text(String),
            Native(OsString),
        }
        Ok(match EncodedPath::deserialize(deserializer)? {
            EncodedPath::Text(text) => text.into(),
            EncodedPath::Native(native) => native.into(),
        })
    }
}
