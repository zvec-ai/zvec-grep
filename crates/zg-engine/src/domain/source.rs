mod encoding;
mod file;
mod format;
mod range;

pub(crate) use file::{FileId, FileSnapshot, SourceFile};
pub(crate) use format::{FileCategory, FileFormat};
pub(crate) use range::{LineColumnRange, SourceRange, TextPosition, TextRange};

pub(crate) use encoding::decode_text;
