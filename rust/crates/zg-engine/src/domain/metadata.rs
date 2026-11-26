use std::borrow::Cow;

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

// Every flag here is an independent fact extraction decided once and the
// resolver reads directly; grouping them would hide which rule reads what.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a separate extracted fact read by a different rule"
)]
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CodeMetadata {
    pub symbol_type: Option<SymbolType>,
    pub symbol_name: Option<String>,
    pub scope: Option<String>,
    /// Declaration syntax without comments, attributes, decorators or implementation bodies.
    /// Literal types and parameter defaults remain part of the declaration.
    pub signature: Option<String>,
    pub documentation: Option<String>,
    pub visibility: Option<Visibility>,
    pub language: Option<Language>,
    /// Declared parameter count of function-shaped symbols, with the
    /// implicit receiver excluded.
    pub arity: Option<usize>,
    /// Whether the symbol is importable from outside its module.
    #[serde(default)]
    pub is_exported: bool,
    /// True for pure declarations (C/C++ prototypes, in-class member
    /// declarations) that never carry a body. Name-based binding prefers the
    /// definition when both exist for the same symbol; a lone declaration is
    /// the only known location and stays bindable.
    #[serde(default)]
    pub is_declaration: bool,
    /// The `package` a Java source declares, empty for the default package.
    /// `None` for languages without the declaration and for indexes written
    /// before the field existed. Import binding needs it because a Java
    /// symbol's name carries no package, so the name alone cannot tell
    /// `my.List` from `java.util.List`.
    #[serde(default)]
    pub package: Option<String>,
    /// True for a C/C++ declaration with internal linkage: a member of an
    /// unnamed namespace, or a file-scope `static` one. Such an entity has a
    /// name in its own translation unit only, so a reference from another
    /// file that matches it by name matched a different declaration.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_internal_linkage: bool,
    /// True for the entity that introduces a type's name (a class, struct,
    /// interface, enum, or trait declaration). An `impl` block carries the
    /// implemented type's name too, so a name index cannot tell the type from
    /// its implementation without this fact; the resolver needs it to name the
    /// one class a member call belongs to.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub declares_type: bool,
    /// Name this symbol is another spelling of, when its initializer is a bare
    /// name (`export const dispatch = send`). A call through the symbol reaches
    /// the aliased one, so an import that binds this symbol must follow the
    /// alias rather than settle on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias_of: Option<String>,
    /// Properties an object literal initializer binds, in the literal's own
    /// order, with a later property replacing an earlier one of its key
    /// (`{ run() {}, run: right }` states `run -> right`). A member call on
    /// this symbol reaches the value its key last named.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub object_members: Vec<MemberAlias>,
    /// Value a later property of the same key replaces this declaration with
    /// (`{ run() {}, run: right }` states `right` on the method's entity). A
    /// call through that key reaches the replacement, not this declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwritten_by: Option<String>,
    /// Names an `export { local as exported }` clause republishes this
    /// declaration under. The clause is a statement of its own, so this is the
    /// only evidence that the exported name reaches this declaration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exported_as: Vec<String>,
}

/// One property of an object literal and the value it binds.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemberAlias {
    pub name: String,
    pub target: String,
}

impl CodeMetadata {
    pub(crate) const SYMBOL_NAME: IndexField = IndexField::String("symbol_name");
    pub(crate) const SYMBOL_TYPE: IndexField = IndexField::String("symbol_type");

    pub(crate) fn declaring_scope(&self) -> Option<&str> {
        // Older Go indexes stored the last generic argument as the method scope.
        // Their signatures still contain the declared receiver, e.g. (q *Queue[Item]).
        if self.language == Some(Language::Go)
            && let Some(receiver) = self.signature.as_deref().and_then(go_generic_receiver)
        {
            return Some(receiver);
        }
        self.scope.as_deref()
    }

    pub(crate) fn qualified_scope(&self) -> Option<Cow<'_, str>> {
        let scope = self.declaring_scope()?;
        Some(match self.package.as_deref() {
            None | Some("") => Cow::Borrowed(scope),
            Some(package) => Cow::Owned(format!("{package}.{scope}")),
        })
    }
}

impl IndexedMetadata for CodeMetadata {
    const INDEX_FIELDS: &'static [IndexField] = &[Self::SYMBOL_NAME, Self::SYMBOL_TYPE];
}

fn go_generic_receiver(signature: &str) -> Option<&str> {
    let declaration = signature
        .strip_prefix("func")?
        .trim_start()
        .strip_prefix('(')?;
    let (receiver, _) = declaration.split_once(')')?;
    let (base, _) = receiver.split_once('[')?;
    base.trim_end()
        .rsplit(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .filter(|name| !name.is_empty())
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

// Keep variants in alphabetical order.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    C,
    Cpp,
    Go,
    Java,
    JavaScript,
    Php,
    Python,
    Rust,
    TypeScript,
}

#[allow(dead_code)]
impl Language {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Go => "go",
            Self::Java => "java",
            Self::JavaScript => "javascript",
            Self::Php => "php",
            Self::Python => "python",
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
        }
    }
}

// Keep variants in alphabetical order.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Internal,
    Private,
    Protected,
    Public,
}

#[allow(dead_code)]
impl Visibility {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Private => "private",
            Self::Protected => "protected",
            Self::Public => "public",
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

#[cfg(test)]
mod tests {
    use super::{CodeMetadata, MemberAlias, SymbolType};

    /// The facts only the resolver reads. Retrieval never looks at them, so
    /// storing a default for each of them would cost every record the fields of
    /// the few files that use them.
    const GRAPH_FACTS: [&str; 6] = [
        "has_internal_linkage",
        "declares_type",
        "alias_of",
        "object_members",
        "overwritten_by",
        "exported_as",
    ];

    #[test]
    fn graph_facts_are_stored_only_when_set() {
        let plain = serde_json::to_value(CodeMetadata::default()).expect("encode metadata");
        for field in GRAPH_FACTS {
            assert!(
                plain.get(field).is_none(),
                "{field} is omitted while it holds its default"
            );
        }

        // A record written before the fields existed decodes to the defaults.
        let older: CodeMetadata = serde_json::from_value(serde_json::json!({"symbol_name": "run"}))
            .expect("decode an older record");
        assert_eq!(
            older,
            CodeMetadata {
                symbol_name: Some("run".to_owned()),
                ..CodeMetadata::default()
            }
        );

        let stated = CodeMetadata {
            symbol_type: Some(SymbolType::Value),
            symbol_name: Some("api".to_owned()),
            has_internal_linkage: true,
            declares_type: true,
            alias_of: Some("send".to_owned()),
            object_members: vec![MemberAlias {
                name: "run".to_owned(),
                target: "right".to_owned(),
            }],
            overwritten_by: Some("right".to_owned()),
            exported_as: vec!["publicApi".to_owned()],
            ..CodeMetadata::default()
        };
        let encoded = serde_json::to_value(&stated).expect("encode stated metadata");
        for field in GRAPH_FACTS {
            assert!(encoded.get(field).is_some(), "{field} is stored when set");
        }
        assert_eq!(
            serde_json::from_value::<CodeMetadata>(encoded).expect("decode stated metadata"),
            stated,
            "a stored fact survives the round trip"
        );
    }
}
