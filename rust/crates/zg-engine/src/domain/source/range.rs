use crate::{EngineError, EngineResult};

/// Locates content within its original source file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SourceRange {
    File,
    Byte(ByteRange),
    Text(TextRange),
}

impl SourceRange {
    #[track_caller]
    pub(crate) fn validate(&self) -> EngineResult<()> {
        match self {
            Self::File => Ok(()),
            Self::Byte(range) => range.validate(),
            Self::Text(range) => range.validate(),
        }
    }

    /// Assumes both ranges refer to the same source file.
    pub(crate) fn contains(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::File, _) => other.validate().is_ok(),
            (Self::Byte(outer), Self::Byte(inner)) => outer.contains(inner),
            (Self::Text(outer), Self::Text(inner)) => outer.contains(inner),
            _ => false,
        }
    }
}

/// Zero-based, half-open offsets in the original file bytes; empty spans are valid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ByteRange {
    pub start_offset: u64,
    pub end_offset: u64,
}

impl ByteRange {
    #[track_caller]
    pub(crate) fn validate(&self) -> EngineResult<()> {
        if self.start_offset > self.end_offset {
            return Err(EngineError::invalid_argument(format!(
                "invalid byte range: raw file offsets {}..{} must be ordered",
                self.start_offset, self.end_offset
            )));
        }
        Ok(())
    }

    pub(crate) fn contains(&self, other: &Self) -> bool {
        self.validate().is_ok()
            && other.validate().is_ok()
            && self.start_offset <= other.start_offset
            && self.end_offset >= other.end_offset
    }
}

/// A half-open span in decoded UTF-8 text, with global and line-relative byte positions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TextRange {
    start: TextPosition,
    end: TextPosition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TextPosition {
    byte_offset: usize,
    line: usize,
    byte_column: usize,
}

impl TextRange {
    /// Creates a range from zero-based, half-open UTF-8 byte offsets in `source_text`.
    /// `line_starts` lists every line's zero-based byte offset, sorted and starting with 0.
    /// Returned line numbers are one-based; byte columns are zero-based.
    #[track_caller]
    pub(crate) fn from_offsets(
        source_text: &str,
        line_starts: &[usize],
        start_byte_offset: usize,
        end_byte_offset: usize,
    ) -> EngineResult<Self> {
        if source_text
            .get(start_byte_offset..end_byte_offset)
            .is_none()
        {
            return Err(invalid_text_offsets(
                source_text.len(),
                start_byte_offset,
                end_byte_offset,
            ));
        }
        let position = |byte_offset| {
            let line = line_starts.partition_point(|&start| start <= byte_offset);
            let line_start = *line_starts.get(line.checked_sub(1)?)?;
            if line_starts.first() != Some(&0)
                || (line_start > 0 && source_text.as_bytes().get(line_start - 1) != Some(&b'\n'))
            {
                return None;
            }
            Some(TextPosition {
                byte_offset,
                line,
                byte_column: byte_offset - line_start,
            })
        };
        let (Some(start), Some(end)) = (position(start_byte_offset), position(end_byte_offset))
        else {
            return Err(EngineError::invalid_argument(
                "cannot create text range: line starts must belong to the decoded source",
            ));
        };
        let range = Self { start, end };
        range.validate()?;
        Ok(range)
    }

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
    pub(crate) fn validate(&self) -> EngineResult<()> {
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

    /// Reads an exact span, rejecting out-of-bounds offsets and split UTF-8 characters.
    #[track_caller]
    pub(crate) fn slice<'a>(&self, source_text: &'a str) -> EngineResult<&'a str> {
        source_text
            .get(self.start.byte_offset..self.end.byte_offset)
            .ok_or_else(|| {
                invalid_text_offsets(
                    source_text.len(),
                    self.start.byte_offset,
                    self.end.byte_offset,
                )
            })
    }

    pub(crate) fn contains(&self, other: &Self) -> bool {
        self.start.byte_offset <= other.start.byte_offset
            && self.end.byte_offset >= other.end.byte_offset
    }
}

#[track_caller]
fn invalid_text_offsets(source_len: usize, start: usize, end: usize) -> EngineError {
    EngineError::invalid_argument(format!(
        "cannot read text range {start}..{end}: offsets must be ordered, within the decoded source ({source_len} bytes), and on UTF-8 character boundaries"
    ))
}
