//! Per-language vocabulary tables for graph relation collection.
//!
//! Declarative configuration only — no algorithms. Adding a language means
//! extending these tables, not writing new traversal logic (the same
//! three-tier precision model the code adapters follow: shared heuristics,
//! vocabulary table, targeted hooks).
//!
//! Node type names refer to the tree-sitter grammars linked into the engine
//! and must be kept in sync with grammar upgrades. Mirrors the TypeScript
//! `src/engine/extraction/graph/languages.ts` tables.

use crate::domain::FileFormat;

/// Call-ish node types shared across grammars. Most grammars name call
/// expressions with one of these types, so a single set covers the majority
/// of languages without per-language entries.
pub(super) const COMMON_CALL_TYPES: &[&str] = &[
    "call",
    "call_expression",
    "function_call",
    "function_call_expression",
    "method_call",
    "method_invocation",
    "method_call_expression",
    "invocation",
    "object_creation_expression",
    "new_expression",
    "constructor_invocation",
];

/// Field names that carry the callee expression, tried in order. Grammars
/// conventionally name this field `function`, `callee`, `name`, or
/// `constructor` — the shared field-naming contract that keeps the collector
/// language-agnostic.
pub(super) const CALLEE_FIELDS: &[&str] = &["function", "callee", "name", "constructor"];

/// Import vocabulary for one language.
pub(super) struct ImportVocabulary {
    /// Node types that declare imports.
    pub(super) import_types: &'static [&'static str],
    /// Grammar field names holding the module path, tried in order. When none
    /// matches, the collector falls back to parsing the node text (needed for
    /// Rust, whose `use` declarations carry no path field).
    pub(super) module_fields: &'static [&'static str],
}

const JS_IMPORT_VOCABULARY: ImportVocabulary = ImportVocabulary {
    import_types: &["import_statement"],
    module_fields: &["source"],
};

const C_IMPORT_VOCABULARY: ImportVocabulary = ImportVocabulary {
    import_types: &["preproc_include"],
    module_fields: &["path"],
};

/// Import vocabulary per language format. Languages without an entry have no
/// import mechanism the collector recognizes (references still resolve via
/// same-file and workspace-unique evidence later).
pub(super) fn import_vocabulary_for(format: FileFormat) -> Option<ImportVocabulary> {
    let vocabulary = match format {
        FileFormat::TypeScript | FileFormat::JavaScript => JS_IMPORT_VOCABULARY,
        FileFormat::Python => ImportVocabulary {
            import_types: &["import_statement", "import_from_statement"],
            module_fields: &["module_name", "name"],
        },
        FileFormat::Go => ImportVocabulary {
            import_types: &["import_declaration", "import_spec"],
            // The field lives on the nested `import_spec` nodes; the
            // collector descends when the declaration itself has none.
            module_fields: &["path"],
        },
        FileFormat::Java => ImportVocabulary {
            import_types: &["import_declaration"],
            // `import_declaration` carries no path field; the collector reads
            // the first identifier-like named child instead.
            module_fields: &[],
        },
        FileFormat::C | FileFormat::Cpp => C_IMPORT_VOCABULARY,
        FileFormat::Rust => ImportVocabulary {
            import_types: &["use_declaration_list", "use_declaration"],
            // `use a::b::{c, d};` has no single path field — text fallback
            // applies.
            module_fields: &[],
        },
        _ => return None,
    };
    Some(vocabulary)
}

/// How a language attaches inheritance clauses to a class-like entity.
///
/// The TypeScript tables keyed on grammar *fields* (`superclass`,
/// `base_clause`), but the grammar crates linked here expose several of those
/// clauses as *nodes* instead (TypeScript `extends_clause` below
/// `class_heritage`, C++ `base_class_clause` below `class_specifier`), so the
/// vocabulary distinguishes the two shapes.
pub(super) enum HeritageVocabulary {
    /// Clause node types found within two levels below the entity (the whole
    /// matched node feeds type-name extraction).
    Nested {
        extends_types: &'static [&'static str],
        implements_types: &'static [&'static str],
    },
    /// Grammar fields on the entity node itself (the field's node feeds
    /// type-name extraction).
    Fields {
        extends: &'static [&'static str],
        implements: &'static [&'static str],
    },
}

/// Inheritance clause vocabulary per language. Go's implicit interface
/// satisfaction is intentionally absent: there is no syntactic `implements`
/// clause to collect.
pub(super) fn heritage_vocabulary_for(format: FileFormat) -> Option<HeritageVocabulary> {
    let vocabulary = match format {
        FileFormat::TypeScript => HeritageVocabulary::Nested {
            extends_types: &["extends_clause"],
            implements_types: &["implements_clause"],
        },
        FileFormat::JavaScript => HeritageVocabulary::Nested {
            // `class_heritage` wraps the extended expression directly.
            extends_types: &["class_heritage"],
            implements_types: &[],
        },
        FileFormat::Python => HeritageVocabulary::Fields {
            extends: &["superclasses"],
            implements: &[],
        },
        FileFormat::Java => HeritageVocabulary::Fields {
            extends: &["superclass"],
            implements: &["interfaces"],
        },
        FileFormat::C | FileFormat::Cpp => HeritageVocabulary::Nested {
            extends_types: &["base_class_clause"],
            implements_types: &[],
        },
        _ => return None,
    };
    Some(vocabulary)
}
