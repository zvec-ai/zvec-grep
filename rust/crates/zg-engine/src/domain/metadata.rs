use serde::{Deserialize, Serialize};

// Keep variants in alphabetical order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntityMetadata {
    Code(CodeMetadata),
    Markdown(MarkdownMetadata),
}

impl EntityMetadata {
    /// Returns the metadata fields stored separately in the underlying storage
    /// and available for query filtering across all entity kinds.
    pub(crate) fn index_schema() -> impl Iterator<Item = IndexField> {
        [CodeMetadata::INDEX_FIELDS, MarkdownMetadata::INDEX_FIELDS]
            .into_iter()
            .flatten()
            .copied()
    }
}

// --- Index ---

/// Declares the fields available to metadata filters.
pub(crate) trait IndexedMetadata {
    const INDEX_FIELDS: &'static [IndexField];
}

/// A metadata field stored separately in the underlying storage for query filtering.
///
/// The field name must match its serialized JSON key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndexField {
    String(&'static str),
}

impl IndexField {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::String(name) => name,
        }
    }
}

// --- Code ---

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CodeMetadata {
    pub symbol_type: Option<SymbolType>,
    pub symbol_name: Option<String>,
    pub scope: Option<String>,
    pub signature: Option<String>,
    pub documentation: Option<String>,
}

impl CodeMetadata {
    pub(crate) const SYMBOL_NAME: IndexField = IndexField::String("symbol_name");
    pub(crate) const SYMBOL_TYPE: IndexField = IndexField::String("symbol_type");
}

impl IndexedMetadata for CodeMetadata {
    const INDEX_FIELDS: &'static [IndexField] = &[Self::SYMBOL_NAME, Self::SYMBOL_TYPE];
}

// Keep variants in alphabetical order.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolType {
    Alias,
    Class,
    Enum,
    Function,
    Interface,
    Module,
    Value,
}

impl SymbolType {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Alias => "alias",
            Self::Class => "class",
            Self::Enum => "enum",
            Self::Function => "function",
            Self::Interface => "interface",
            Self::Module => "module",
            Self::Value => "value",
        }
    }
}

// --- Markdown ---

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MarkdownMetadata {
    pub heading: Option<String>,
    pub level: Option<usize>,
    pub scope: Option<String>,
}

impl IndexedMetadata for MarkdownMetadata {
    const INDEX_FIELDS: &'static [IndexField] = &[];
}
