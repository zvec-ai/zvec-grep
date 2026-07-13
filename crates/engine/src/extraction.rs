use std::path::Path;

use crate::{
    CodeSymbol, EngineError, FileFormat, FileId, FileLocation, FileSegment, Result, SegmentKind,
    file::ScannedFile,
};

mod code;

const DEFAULT_CHUNK_CHARS: usize = 3_600;
const DEFAULT_OVERLAP_LINES: usize = 3;

#[derive(Debug, Clone)]
pub struct Extractor {
    max_chunk_chars: usize,
    overlap_lines: usize,
}

impl Default for Extractor {
    fn default() -> Self {
        Self {
            max_chunk_chars: DEFAULT_CHUNK_CHARS,
            overlap_lines: DEFAULT_OVERLAP_LINES,
        }
    }
}

impl Extractor {
    pub fn new(max_chunk_chars: usize, overlap_lines: usize) -> Result<Self> {
        if max_chunk_chars == 0 {
            return Err(EngineError::InvalidConfig(
                "chunk size must be positive".into(),
            ));
        }
        Ok(Self {
            max_chunk_chars,
            overlap_lines,
        })
    }

    pub(crate) fn extract(&self, file: &ScannedFile, bytes: &[u8]) -> Result<Vec<FileSegment>> {
        if !file.format.is_text() {
            return Ok(Vec::new());
        }
        if looks_binary(bytes) {
            return Ok(Vec::new());
        }
        let text = std::str::from_utf8(bytes).map_err(|error| EngineError::Extraction {
            path: file.relative_path.clone(),
            message: error.to_string(),
        })?;
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }

        if file.format == FileFormat::Markdown {
            Ok(self.extract_markdown(file, text))
        } else if segment_kind(file.format) == SegmentKind::Code {
            Ok(self.extract_code(file, text))
        } else {
            Ok(self.extract_lines(file, text, segment_kind(file.format)))
        }
    }

    fn extract_code(&self, file: &ScannedFile, text: &str) -> Vec<FileSegment> {
        let lines: Vec<&str> = text.lines().collect();
        let Some(ranges) = code::syntax_ranges(file, text, self.max_chunk_chars) else {
            return self.chunks_for_lines(
                &file.id,
                &file.relative_path,
                &lines,
                0,
                SegmentKind::Code,
                None,
            );
        };
        ranges
            .into_iter()
            .flat_map(|unit| {
                self.chunks_for_lines(
                    &file.id,
                    &file.relative_path,
                    &lines[unit.lines.start..unit.lines.end],
                    unit.lines.start,
                    SegmentKind::Code,
                    unit.symbol,
                )
            })
            .collect()
    }

    fn extract_markdown(&self, file: &ScannedFile, text: &str) -> Vec<FileSegment> {
        let lines: Vec<&str> = text.lines().collect();
        let mut starts = vec![0];
        let mut in_fence = false;
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                in_fence = !in_fence;
            } else if !in_fence && index > 0 && trimmed.starts_with('#') {
                starts.push(index);
            }
        }
        starts.push(lines.len());

        let mut segments = Vec::new();
        for window in starts.windows(2) {
            let start = window[0];
            let end = window[1];
            let section = lines[start..end].join("\n");
            if section.trim().is_empty() {
                continue;
            }
            segments.extend(self.chunks_for_lines(
                &file.id,
                &file.relative_path,
                &lines[start..end],
                start,
                SegmentKind::Section,
                None,
            ));
        }
        segments
    }

    fn extract_lines(&self, file: &ScannedFile, text: &str, kind: SegmentKind) -> Vec<FileSegment> {
        let lines: Vec<&str> = text.lines().collect();
        self.chunks_for_lines(&file.id, &file.relative_path, &lines, 0, kind, None)
    }

    fn chunks_for_lines(
        &self,
        file_id: &FileId,
        relative_path: &Path,
        lines: &[&str],
        line_offset: usize,
        kind: SegmentKind,
        symbol: Option<CodeSymbol>,
    ) -> Vec<FileSegment> {
        let mut segments = Vec::new();
        let mut start = 0;
        while start < lines.len() {
            let mut end = start;
            let mut chars = 0;
            while end < lines.len() {
                let next = lines[end].len() + usize::from(end + 1 < lines.len());
                if end > start && chars + next > self.max_chunk_chars {
                    break;
                }
                chars += next;
                end += 1;
                if chars >= self.max_chunk_chars {
                    break;
                }
            }
            if end == start {
                end += 1;
            }
            let content = lines[start..end].join("\n");
            if !content.trim().is_empty() {
                let start_line = line_offset + start + 1;
                let end_line = line_offset + end;
                let segment_key = format!("{}:{start_line}:{end_line}", file_id.as_str());
                segments.push(FileSegment {
                    id: blake3::hash(segment_key.as_bytes()).to_hex().to_string(),
                    file_id: file_id.clone(),
                    relative_path: relative_path.to_path_buf(),
                    kind,
                    symbol: symbol.clone(),
                    location: FileLocation::Lines {
                        start: start_line,
                        end: end_line,
                    },
                    text: content,
                });
            }
            if end >= lines.len() {
                break;
            }
            start = end.saturating_sub(self.overlap_lines).max(start + 1);
        }
        segments
    }
}

fn segment_kind(format: FileFormat) -> SegmentKind {
    match format {
        FileFormat::Rust
        | FileFormat::TypeScript
        | FileFormat::JavaScript
        | FileFormat::Python
        | FileFormat::Go
        | FileFormat::Java
        | FileFormat::C
        | FileFormat::Cpp => SegmentKind::Code,
        _ => SegmentKind::Text,
    }
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8_192).any(|byte| *byte == 0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn file(format: FileFormat) -> ScannedFile {
        ScannedFile {
            id: FileId::from_relative_path(Path::new("test")),
            absolute_path: PathBuf::from("/test"),
            relative_path: PathBuf::from("test"),
            format,
            size: 0,
            modified_ms: 0,
        }
    }

    #[test]
    fn chunks_text_with_locations_and_overlap() {
        let extractor = Extractor::new(8, 1).unwrap();
        let segments = extractor
            .extract(&file(FileFormat::Text), b"one\ntwo\nthree\nfour")
            .unwrap();
        assert!(segments.len() > 1);
        assert_eq!(
            segments[0].location,
            FileLocation::Lines { start: 1, end: 2 }
        );
        assert!(segments[1].text.starts_with("two"));
    }

    #[test]
    fn markdown_splits_at_headings_but_not_fenced_headings() {
        let text = b"intro\n# One\nbody\n```\n# code\n```\n# Two\nend";
        let segments = Extractor::default()
            .extract(&file(FileFormat::Markdown), text)
            .unwrap();
        assert_eq!(segments.len(), 3);
        assert!(segments[1].text.contains("# code"));
    }

    #[test]
    fn binary_content_is_skipped() {
        assert!(
            Extractor::default()
                .extract(&file(FileFormat::Text), b"a\0b")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn code_extraction_keeps_functions_as_separate_units() {
        let text = b"use std::fs;\n\nfn first() {\n    println!(\"one\");\n}\n\nfn second() {\n    println!(\"two\");\n}\n";
        let segments = Extractor::default()
            .extract(&file(FileFormat::Rust), text)
            .unwrap();
        assert_eq!(segments.len(), 3);
        assert!(segments[1].text.starts_with("fn first"));
        assert!(segments[2].text.starts_with("fn second"));
        assert_eq!(segments[1].symbol.as_ref().unwrap().name, "first");
    }
}
