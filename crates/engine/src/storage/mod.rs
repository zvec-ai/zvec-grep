mod memory;
mod zvec;

use crate::{FileId, FileSegment, IndexedFile, Result};

pub use memory::InMemoryStorage;
pub use zvec::ZvecStorage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageStats {
    pub files: usize,
    pub segments: usize,
}

#[derive(Debug, Clone)]
pub struct RecallHit {
    pub segment: FileSegment,
    pub file: IndexedFile,
    pub score: f32,
}

/// Persistence boundary used by the engine. Implementations can use Zvec,
/// memory, or a future platform-specific store without changing indexing.
pub trait Storage: Send {
    fn stats(&self) -> Result<StorageStats>;
    fn files(&self) -> Result<Vec<IndexedFile>>;
    fn replace_file(
        &mut self,
        file: IndexedFile,
        segments: Vec<FileSegment>,
        vectors: Vec<Vec<f32>>,
    ) -> Result<()>;
    fn replace_files(
        &mut self,
        files: Vec<(IndexedFile, Vec<FileSegment>, Vec<Vec<f32>>)>,
    ) -> Result<()> {
        for (file, segments, vectors) in files {
            self.replace_file(file, segments, vectors)?;
        }
        Ok(())
    }
    fn update_file(&mut self, file: IndexedFile) -> Result<()>;
    fn remove_file(&mut self, id: &FileId) -> Result<()>;
    fn remove_files(&mut self, ids: &[FileId]) -> Result<()> {
        for id in ids {
            self.remove_file(id)?;
        }
        Ok(())
    }
    /// Makes writes durable. Indexing calls this once per bounded batch.
    fn commit(&mut self) -> Result<()> {
        Ok(())
    }
    fn lexical_recall(&self, query: &str, limit: usize) -> Result<Vec<RecallHit>>;
    fn vector_recall(&self, vector: &[f32], limit: usize) -> Result<Vec<RecallHit>>;
}
