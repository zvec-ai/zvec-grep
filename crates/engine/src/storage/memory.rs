use std::collections::HashMap;

use crate::{EngineError, FileId, FileSegment, IndexedFile, Result};

use super::{RecallHit, Storage, StorageStats};

#[derive(Default)]
pub struct InMemoryStorage {
    files: HashMap<FileId, IndexedFile>,
    segments: HashMap<FileId, Vec<FileSegment>>,
    vectors: HashMap<FileId, Vec<Vec<f32>>>,
}

impl InMemoryStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Storage for InMemoryStorage {
    fn stats(&self) -> Result<StorageStats> {
        Ok(StorageStats {
            files: self.files.len(),
            segments: self.segments.values().map(Vec::len).sum(),
        })
    }

    fn files(&self) -> Result<Vec<IndexedFile>> {
        Ok(self.files.values().cloned().collect())
    }

    fn replace_file(
        &mut self,
        file: IndexedFile,
        segments: Vec<FileSegment>,
        vectors: Vec<Vec<f32>>,
    ) -> Result<()> {
        if segments.len() != vectors.len() {
            return Err(EngineError::Storage(
                "segment and vector counts do not match".into(),
            ));
        }
        let id = file.id.clone();
        self.files.insert(id.clone(), file);
        self.segments.insert(id.clone(), segments);
        self.vectors.insert(id, vectors);
        Ok(())
    }

    fn update_file(&mut self, file: IndexedFile) -> Result<()> {
        self.files.insert(file.id.clone(), file);
        Ok(())
    }

    fn remove_file(&mut self, id: &FileId) -> Result<()> {
        self.files.remove(id);
        self.segments.remove(id);
        self.vectors.remove(id);
        Ok(())
    }

    fn lexical_recall(&self, query: &str, limit: usize) -> Result<Vec<RecallHit>> {
        let terms: Vec<_> = query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        let mut hits = Vec::new();
        for segments in self.segments.values() {
            for segment in segments {
                let haystack = format!(
                    "{} {} {}",
                    segment.relative_path.display(),
                    segment
                        .symbol
                        .as_ref()
                        .map_or("", |symbol| symbol.name.as_str()),
                    segment.text
                )
                .to_ascii_lowercase();
                let score = terms
                    .iter()
                    .filter(|term| haystack.contains(term.as_str()))
                    .count() as f32;
                if score > 0.0 {
                    hits.push(RecallHit {
                        segment: segment.clone(),
                        file: self.files[&segment.file_id].clone(),
                        score,
                    });
                }
            }
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(limit);
        Ok(hits)
    }

    fn vector_recall(&self, vector: &[f32], limit: usize) -> Result<Vec<RecallHit>> {
        let mut hits = Vec::new();
        for (file_id, vectors) in &self.vectors {
            let Some(segments) = self.segments.get(file_id) else {
                continue;
            };
            for (segment, candidate) in segments.iter().zip(vectors) {
                let score = vector.iter().zip(candidate).map(|(a, b)| a * b).sum();
                hits.push(RecallHit {
                    segment: segment.clone(),
                    file: self.files[file_id].clone(),
                    score,
                });
            }
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(limit);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::{FileFormat, FileLocation, SegmentKind};

    use super::*;

    #[test]
    fn replaces_and_recalls_segments() {
        let id = FileId::from_relative_path(Path::new("src/lib.rs"));
        let file = IndexedFile {
            id: id.clone(),
            relative_path: PathBuf::from("src/lib.rs"),
            format: FileFormat::Rust,
            size: 10,
            modified_ms: 1,
            content_hash: "hash".into(),
        };
        let segment = FileSegment {
            id: "segment".into(),
            file_id: id,
            relative_path: PathBuf::from("src/lib.rs"),
            kind: SegmentKind::Code,
            symbol: None,
            location: FileLocation::Lines { start: 1, end: 1 },
            text: "parse source file".into(),
        };
        let mut storage = InMemoryStorage::new();
        storage
            .replace_file(file, vec![segment], vec![vec![1.0, 0.0]])
            .unwrap();
        assert_eq!(storage.lexical_recall("source", 10).unwrap().len(), 1);
        assert_eq!(storage.vector_recall(&[1.0, 0.0], 10).unwrap().len(), 1);
    }
}
