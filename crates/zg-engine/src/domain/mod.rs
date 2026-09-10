// TODO: Remove these temporary lint allowances once workflows use the domain types.
#![allow(
    dead_code,
    unused_imports,
    reason = "Domain types are staged before workflow migration."
)]

mod content;
mod entity;
mod source;

pub(crate) use content::{Content, ImageContent, TableCell, TableCellKind, TableContent};
pub(crate) use entity::{
    Entity, EntityContent, EntityFragment, EntityId, EntityMetadata, FragmentId, SymbolType,
    WindowFragment, validate_fragments,
};
pub(crate) use source::{
    FileCategory, FileFormat, FileId, FileSnapshot, LineColumnRange, SourceFile, SourceRange,
    TextPosition, TextRange,
};
