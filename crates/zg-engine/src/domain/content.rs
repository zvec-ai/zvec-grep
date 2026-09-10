use super::source::FileFormat;

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
    pub data: Vec<u8>,
    pub format: FileFormat,
}

// --- Table ---

/// A logical table, independent of its source format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TableContent {
    pub row_count: usize,
    pub column_count: usize,
    /// Cells in row-major order. Merged cells appear once at their top-left anchor.
    /// Uncovered positions without a cell have no recorded value.
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
    pub text: String,
    pub kind: TableCellKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TableCellKind {
    /// The source or extraction result does not establish the cell's role.
    Unknown,
    Data,
    Header,
}
