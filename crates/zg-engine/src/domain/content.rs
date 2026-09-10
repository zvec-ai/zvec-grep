use crate::{EngineError, EngineResult};

use super::source::{FileCategory, FileFormat};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Content {
    Text(String),
    Image(ImageContent),
    Table(TableContent),
}

// --- Image ---

/// Stores a complete encoded image resource and its format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ImageContent {
    data: Vec<u8>,
    format: FileFormat,
}

impl ImageContent {
    #[track_caller]
    pub(crate) fn new(data: Vec<u8>, format: FileFormat) -> EngineResult<Self> {
        if !format.categories().contains(&FileCategory::Image) {
            return Err(EngineError::invalid_argument(format!(
                "image content requires an image format, got {}",
                format.as_str()
            )));
        }
        if data.is_empty() {
            return Err(EngineError::invalid_argument(
                "image content requires non-empty data",
            ));
        }
        Ok(Self { data, format })
    }

    pub(crate) fn format(&self) -> FileFormat {
        self.format
    }

    pub(crate) fn data(&self) -> &[u8] {
        &self.data
    }
}

// --- Table ---

/// A logical table, independent of its source format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TableContent {
    pub row_count: usize,
    pub column_count: usize,
    /// Cells in row-major order. Merged cells are stored once at the top-left.
    /// Positions not covered by a cell have no recorded content.
    pub cells: Vec<TableCell>,
}

/// Table-local, zero-based coordinates with positive spans.
/// Cells must stay within the table bounds and must not overlap.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TableCell {
    pub row: usize,
    pub column: usize,
    pub row_span: usize,
    pub column_span: usize,
    /// Contents in reading order.
    pub contents: Vec<Content>,
    pub kind: TableCellRole,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TableCellRole {
    Unknown,
    Data,
    Header,
}
