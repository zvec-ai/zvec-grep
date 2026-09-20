use crate::{EngineError, EngineResult};

/// Locates an entity in its source or a fragment within its entity's content.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Range {
    Full,
    Byte(ByteRange),
    Text(TextRange),
}

impl Range {
    pub(crate) fn contains(&self, other: &Self) -> EngineResult<bool> {
        match (self, other) {
            (Self::Full, Self::Full) => Ok(true),
            (Self::Byte(outer), Self::Byte(inner)) => Ok(
                outer.start_offset <= inner.start_offset && outer.end_offset >= inner.end_offset
            ),
            (Self::Text(outer), Self::Text(inner)) => Ok(outer.start.byte_offset
                <= inner.start.byte_offset
                && outer.end.byte_offset >= inner.end.byte_offset),
            _ => Err(EngineError::invalid_argument(
                "cannot compare ranges of different kinds",
            )),
        }
    }
}

/// Zero-based, half-open byte offsets in the containing source or content.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct ByteRange {
    start_offset: u64,
    end_offset: u64,
}

impl ByteRange {
    #[track_caller]
    pub(crate) fn new(start_offset: u64, end_offset: u64) -> EngineResult<Self> {
        if start_offset > end_offset {
            return Err(EngineError::invalid_argument(format!(
                "invalid byte range: offsets {start_offset}..{end_offset} must be ordered",
            )));
        }
        Ok(Self {
            start_offset,
            end_offset,
        })
    }

    pub(crate) fn start_offset(&self) -> u64 {
        self.start_offset
    }

    pub(crate) fn end_offset(&self) -> u64 {
        self.end_offset
    }
}

/// A half-open location in decoded UTF-8 source text, with one-based lines and
/// zero-based byte offsets and columns.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct TextRange {
    start: TextPosition,
    end: TextPosition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
struct TextPosition {
    byte_offset: usize,
    line: usize,
    byte_column: usize,
}

impl TextRange {
    /// Checks recorded coordinates without reading the source.
    #[track_caller]
    pub(crate) fn from_coordinates(
        start_byte_offset: usize,
        end_byte_offset: usize,
        start_line: usize,
        end_line: usize,
        start_byte_column: usize,
        end_byte_column: usize,
    ) -> EngineResult<Self> {
        let range = Self {
            start: TextPosition {
                byte_offset: start_byte_offset,
                line: start_line,
                byte_column: start_byte_column,
            },
            end: TextPosition {
                byte_offset: end_byte_offset,
                line: end_line,
                byte_column: end_byte_column,
            },
        };
        range.validate()?;
        Ok(range)
    }

    pub(crate) fn start_byte_offset(&self) -> usize {
        self.start.byte_offset
    }

    pub(crate) fn end_byte_offset(&self) -> usize {
        self.end.byte_offset
    }

    pub(crate) fn start_line(&self) -> usize {
        self.start.line
    }

    pub(crate) fn end_line(&self) -> usize {
        self.end.line
    }

    pub(crate) fn start_byte_column(&self) -> usize {
        self.start.byte_column
    }

    pub(crate) fn end_byte_column(&self) -> usize {
        self.end.byte_column
    }

    #[track_caller]
    fn validate(&self) -> EngineResult<()> {
        let valid_position = |position: TextPosition| {
            position.line > 0
                && position.byte_column <= position.byte_offset
                && if position.line == 1 {
                    position.byte_column == position.byte_offset
                } else {
                    position.byte_offset - position.byte_column >= position.line - 1
                }
        };
        let consistent = valid_position(self.start)
            && valid_position(self.end)
            && self.start.byte_offset <= self.end.byte_offset
            && self.start.line <= self.end.line
            && if self.start.line == self.end.line {
                self.start.byte_offset - self.start.byte_column
                    == self.end.byte_offset - self.end.byte_column
            } else {
                (self.end.byte_offset - self.end.byte_column)
                    .checked_sub(self.start.byte_offset)
                    .is_some_and(|distance| distance >= self.end.line - self.start.line)
            };
        if !consistent {
            return Err(EngineError::invalid_argument(format!(
                "invalid text range: {:?}..{:?}; UTF-8 byte offsets, one-based lines, and byte columns must be ordered and consistent",
                self.start, self.end
            )));
        }
        Ok(())
    }
}
