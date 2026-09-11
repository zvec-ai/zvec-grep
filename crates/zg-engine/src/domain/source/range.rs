use crate::{EngineError, EngineResult};

/// Lines are one-based and inclusive; source-global UTF-16 offsets are zero-based
/// and half-open. Empty offset spans are valid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TextRange {
    pub start_line: usize,
    pub end_line: usize,
    pub start_utf16_offset: usize,
    pub end_utf16_offset: usize,
}

impl TextRange {
    #[track_caller]
    pub(crate) fn validate(&self) -> EngineResult<()> {
        if self.start_line == 0
            || self.start_line > self.end_line
            || self.start_utf16_offset > self.end_utf16_offset
        {
            return Err(EngineError::invalid_argument(format!(
                "invalid text range: lines {}..={} must be one-based and ordered; global UTF-16 offsets {}..{} must ascend",
                self.start_line, self.end_line, self.start_utf16_offset, self.end_utf16_offset
            )));
        }
        Ok(())
    }

    pub(crate) fn contains(&self, other: &Self) -> bool {
        self.validate().is_ok()
            && other.validate().is_ok()
            && self.start_line <= other.start_line
            && self.end_line >= other.end_line
            && self.start_utf16_offset <= other.start_utf16_offset
            && self.end_utf16_offset >= other.end_utf16_offset
    }
}

/// A one-based line and zero-based UTF-16 column within that line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TextPosition {
    pub line: usize,
    pub column_utf16: usize,
}

/// A half-open span in line/column coordinates, independent of global offsets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LineColumnRange {
    pub start: TextPosition,
    pub end: TextPosition,
}

impl LineColumnRange {
    #[track_caller]
    pub(crate) fn validate(&self) -> EngineResult<()> {
        if self.start.line == 0
            || self.end.line == 0
            || (self.start.line, self.start.column_utf16) > (self.end.line, self.end.column_utf16)
        {
            return Err(EngineError::invalid_argument(format!(
                "invalid line/column range: {:?}..{:?}; lines must be one-based and UTF-16 positions must ascend",
                self.start, self.end
            )));
        }
        Ok(())
    }

    pub(crate) fn contains(&self, other: &Self) -> bool {
        self.validate().is_ok()
            && other.validate().is_ok()
            && (self.start.line, self.start.column_utf16)
                <= (other.start.line, other.start.column_utf16)
            && (self.end.line, self.end.column_utf16) >= (other.end.line, other.end.column_utf16)
    }
}

/// Ranges refer to one source. Pages are one-based; byte and page-local UTF-16
/// spans are zero-based and half-open. Regions use source-defined page units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SourceRange {
    File,
    Text(TextRange),
    Byte {
        start_offset: u64,
        end_offset: u64,
    },
    Page {
        page: usize,
    },
    PageText {
        page: usize,
        start_utf16_offset: usize,
        end_utf16_offset: usize,
    },
    PageRegion {
        page: usize,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
}

impl SourceRange {
    #[track_caller]
    pub(crate) fn validate(&self) -> EngineResult<()> {
        let valid = match *self {
            Self::File => true,
            Self::Text(range) => return range.validate(),
            Self::Byte {
                start_offset,
                end_offset,
            } => start_offset <= end_offset,
            Self::Page { page } => page > 0,
            Self::PageText {
                page,
                start_utf16_offset,
                end_utf16_offset,
            } => page > 0 && start_utf16_offset <= end_utf16_offset,
            Self::PageRegion {
                page,
                x,
                y,
                width,
                height,
            } => {
                page > 0
                    && width > 0
                    && height > 0
                    && x.checked_add(width).is_some()
                    && y.checked_add(height).is_some()
            }
        };
        if valid {
            Ok(())
        } else {
            Err(EngineError::invalid_argument(format!(
                "invalid source range: {self:?}"
            )))
        }
    }

    pub(crate) fn contains(&self, other: &Self) -> bool {
        if self.validate().is_err() || other.validate().is_err() {
            return false;
        }
        match (*self, *other) {
            (Self::File, _) => true,
            (Self::Text(outer), Self::Text(inner)) => outer.contains(&inner),
            (
                Self::Byte {
                    start_offset,
                    end_offset,
                },
                Self::Byte {
                    start_offset: other_start,
                    end_offset: other_end,
                },
            ) => start_offset <= other_start && end_offset >= other_end,
            (
                Self::Page { page },
                Self::Page { page: other_page }
                | Self::PageText {
                    page: other_page, ..
                }
                | Self::PageRegion {
                    page: other_page, ..
                },
            ) => page == other_page,
            (
                Self::PageText {
                    page,
                    start_utf16_offset,
                    end_utf16_offset,
                },
                Self::PageText {
                    page: other_page,
                    start_utf16_offset: other_start,
                    end_utf16_offset: other_end,
                },
            ) => {
                page == other_page
                    && start_utf16_offset <= other_start
                    && end_utf16_offset >= other_end
            }
            (
                Self::PageRegion {
                    page,
                    x,
                    y,
                    width,
                    height,
                },
                Self::PageRegion {
                    page: other_page,
                    x: other_x,
                    y: other_y,
                    width: other_width,
                    height: other_height,
                },
            ) => {
                page == other_page
                    && x <= other_x
                    && y <= other_y
                    && u64::from(x) + u64::from(width)
                        >= u64::from(other_x) + u64::from(other_width)
                    && u64::from(y) + u64::from(height)
                        >= u64::from(other_y) + u64::from(other_height)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: (usize, usize), offsets: (usize, usize)) -> TextRange {
        TextRange {
            start_line: lines.0,
            end_line: lines.1,
            start_utf16_offset: offsets.0,
            end_utf16_offset: offsets.1,
        }
    }

    fn bytes(start_offset: u64, end_offset: u64) -> SourceRange {
        SourceRange::Byte {
            start_offset,
            end_offset,
        }
    }

    fn page_text(page: usize, start_utf16_offset: usize, end_utf16_offset: usize) -> SourceRange {
        SourceRange::PageText {
            page,
            start_utf16_offset,
            end_utf16_offset,
        }
    }

    fn region(page: usize, position: (u32, u32), size: (u32, u32)) -> SourceRange {
        SourceRange::PageRegion {
            page,
            x: position.0,
            y: position.1,
            width: size.0,
            height: size.1,
        }
    }

    fn line_columns(start: (usize, usize), end: (usize, usize)) -> LineColumnRange {
        LineColumnRange {
            start: TextPosition {
                line: start.0,
                column_utf16: start.1,
            },
            end: TextPosition {
                line: end.0,
                column_utf16: end.1,
            },
        }
    }

    #[test]
    fn ranges_validate_coordinate_boundaries_and_region_overflow() {
        for (range, valid) in [
            (SourceRange::File, true),
            (SourceRange::Text(text((1, 2), (0, 8))), true),
            (SourceRange::Text(text((1, 1), (0, 0))), true),
            (SourceRange::Text(text((0, 2), (0, 8))), false),
            (SourceRange::Text(text((3, 2), (0, 8))), false),
            (SourceRange::Text(text((1, 2), (9, 8))), false),
            (bytes(u64::MAX, u64::MAX), true),
            (bytes(1, 0), false),
            (SourceRange::Page { page: 1 }, true),
            (SourceRange::Page { page: 0 }, false),
            (page_text(1, 0, 0), true),
            (page_text(0, 0, 1), false),
            (page_text(1, 2, 1), false),
            (region(1, (u32::MAX - 1, u32::MAX - 1), (1, 1)), true),
            (region(0, (0, 0), (1, 1)), false),
            (region(1, (0, 0), (0, 1)), false),
            (region(1, (0, 0), (1, 0)), false),
            (region(1, (u32::MAX, 0), (1, 1)), false),
            (region(1, (0, u32::MAX), (1, 1)), false),
        ] {
            assert_eq!(range.validate().is_ok(), valid, "{range:?}");
            assert_eq!(range.contains(&range), valid, "{range:?}");
            assert_eq!(SourceRange::File.contains(&range), valid, "{range:?}");
        }
    }

    #[test]
    fn containment_checks_both_boundaries_and_page_identity() {
        let outer_text = SourceRange::Text(text((1, 3), (0, 10)));
        let outer_region = region(2, (10, 20), (30, 40));
        for (outer, inner, contained) in [
            (outer_text, SourceRange::Text(text((2, 2), (2, 6))), true),
            (outer_text, SourceRange::Text(text((3, 3), (10, 10))), true),
            (outer_text, SourceRange::Text(text((4, 4), (2, 6))), false),
            (outer_text, SourceRange::Text(text((1, 3), (0, 11))), false),
            (bytes(0, 10), bytes(10, 10), true),
            (bytes(0, 10), bytes(10, 11), false),
            (page_text(2, 0, 10), page_text(2, 2, 8), true),
            (page_text(2, 0, 10), page_text(2, 0, 11), false),
            (page_text(2, 0, 10), page_text(1, 2, 8), false),
            (outer_region, region(2, (20, 30), (20, 30)), true),
            (outer_region, region(2, (20, 30), (21, 30)), false),
            (outer_region, region(2, (20, 30), (20, 31)), false),
            (outer_region, region(2, (9, 20), (1, 1)), false),
            (outer_region, region(2, (10, 19), (1, 1)), false),
            (outer_region, region(1, (20, 30), (20, 30)), false),
            (outer_region, region(2, (u32::MAX, 30), (20, 30)), false),
        ] {
            assert_eq!(
                outer.contains(&inner),
                contained,
                "{outer:?} contains {inner:?}"
            );
        }
    }

    #[test]
    fn containment_requires_matching_coordinate_spaces() {
        let ranges = [
            SourceRange::Text(text((1, 1), (0, 10))),
            bytes(0, 10),
            page_text(2, 0, 10),
            region(2, (0, 0), (10, 10)),
        ];
        for (outer_index, outer) in ranges.iter().enumerate() {
            for (inner_index, inner) in ranges.iter().enumerate() {
                assert_eq!(outer.contains(inner), outer_index == inner_index);
            }
            assert!(!outer.contains(&SourceRange::File));
            assert_eq!(
                SourceRange::Page { page: 2 }.contains(outer),
                outer_index >= 2
            );
            assert!(!SourceRange::Page { page: 1 }.contains(outer));
        }
    }

    #[test]
    fn line_column_ranges_compare_multiline_positions_lexicographically() {
        let outer = line_columns((2, 10), (4, 2));
        let inner = line_columns((3, 0), (3, 100));
        assert!(outer.validate().is_ok());
        assert!(outer.contains(&inner));
        assert!(!inner.contains(&outer));
        assert!(outer.contains(&line_columns((4, 2), (4, 2))));
        for range in [
            line_columns((4, 2), (2, 10)),
            line_columns((0, 0), (4, 2)),
            line_columns((2, 10), (2, 9)),
        ] {
            assert_eq!(
                range.validate().expect_err("invalid range").code(),
                EngineError::INVALID_ARGUMENT
            );
            assert!(!outer.contains(&range));
        }
    }
}
