use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use zvec_rust::{Collection, Doc, Fts, SearchQuery};

use crate::{
    CodeSymbol, CodeSymbolKind, EmbeddingSchema, EngineError, FileId, FileLocation, FileSegment,
    IndexedFile, Result, SegmentKind,
};

use super::{RecallHit, Storage, StorageStats};

mod codec;
use codec::{schema, segment_doc};

const MANIFEST_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    dimension: usize,
    model: String,
    files: HashMap<FileId, IndexedFile>,
    segment_ids: HashMap<FileId, Vec<String>>,
}

impl Manifest {
    fn new(embedding: &EmbeddingSchema) -> Self {
        Self {
            version: MANIFEST_VERSION,
            dimension: embedding.dimension,
            model: embedding.model.clone(),
            files: HashMap::new(),
            segment_ids: HashMap::new(),
        }
    }
}

/// Persistent storage backed by Zvec's native FTS and vector indexes.
pub struct ZvecStorage {
    collection: Collection,
    manifest_path: PathBuf,
    manifest: Manifest,
    dirty: bool,
    _lock: File,
}

impl ZvecStorage {
    pub fn open(root: impl AsRef<Path>, embedding: &EmbeddingSchema) -> Result<Self> {
        Self::open_inner(root.as_ref(), embedding, false)
    }

    /// Discards any existing derived index and creates a new one.
    pub fn recreate(root: impl AsRef<Path>, embedding: &EmbeddingSchema) -> Result<Self> {
        Self::open_inner(root.as_ref(), embedding, true)
    }

    fn open_inner(root: &Path, embedding: &EmbeddingSchema, recreate: bool) -> Result<Self> {
        ensure_initialized()?;
        let parent = root
            .parent()
            .ok_or_else(|| storage_error("index has no parent directory"))?;
        fs::create_dir_all(parent).map_err(|source| EngineError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let lock_path = root.with_extension("lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|source| EngineError::Io {
                path: lock_path.clone(),
                source,
            })?;
        lock.try_lock_exclusive()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::WouldBlock => EngineError::Busy,
                _ => EngineError::Io {
                    path: lock_path,
                    source: error,
                },
            })?;
        if recreate && root.exists() {
            fs::remove_dir_all(root).map_err(|source| EngineError::Io {
                path: root.to_path_buf(),
                source,
            })?;
        }
        fs::create_dir_all(root).map_err(|source| EngineError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let manifest_path = root.join("manifest.json");
        let manifest = if manifest_path.exists() {
            let bytes = fs::read(&manifest_path).map_err(|source| EngineError::Io {
                path: manifest_path.clone(),
                source,
            })?;
            let manifest: Manifest = serde_json::from_slice(&bytes)
                .map_err(|error| storage_error(format!("invalid manifest: {error}")))?;
            if manifest.version != MANIFEST_VERSION {
                return Err(storage_error(format!(
                    "unsupported index version {}",
                    manifest.version
                )));
            }
            if manifest.dimension != embedding.dimension || manifest.model != embedding.model {
                return Err(storage_error(format!(
                    "index was built with {} ({} dimensions), but the active embedder is {} ({} dimensions); rebuild the index",
                    manifest.model, manifest.dimension, embedding.model, embedding.dimension
                )));
            }
            manifest
        } else {
            Manifest::new(embedding)
        };

        let collection_path = root.join("segments");
        let path = collection_path
            .to_str()
            .ok_or_else(|| storage_error("index path is not valid UTF-8"))?;
        let collection = if collection_path.exists() {
            Collection::open(path, None).map_err(zvec_error)?
        } else {
            let schema = schema(embedding.dimension)?;
            Collection::create_and_open(path, &schema, None).map_err(zvec_error)?
        };
        let storage = Self {
            collection,
            manifest_path,
            manifest,
            dirty: false,
            _lock: lock,
        };
        if !storage.manifest_path.exists() {
            storage.save_manifest()?;
        }
        Ok(storage)
    }

    fn save_manifest(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.manifest)
            .map_err(|error| storage_error(error.to_string()))?;
        let parent = self
            .manifest_path
            .parent()
            .ok_or_else(|| storage_error("manifest has no parent directory"))?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).map_err(|source| EngineError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        temporary
            .write_all(&bytes)
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(|source| EngineError::Io {
                path: temporary.path().to_path_buf(),
                source,
            })?;
        temporary
            .persist(&self.manifest_path)
            .map_err(|error| EngineError::Io {
                path: self.manifest_path.clone(),
                source: error.error,
            })?;
        Ok(())
    }

    fn decode_doc(&self, doc: &Doc) -> Result<RecallHit> {
        let id = required_string(doc, "id")?;
        let file_id = FileId::from_stored(required_string(doc, "file_id")?);
        let relative_path = PathBuf::from(required_string(doc, "path")?);
        let kind = match doc.get_i32("kind").map_err(zvec_error)?.unwrap_or(2) {
            0 => SegmentKind::Code,
            1 => SegmentKind::Section,
            _ => SegmentKind::Text,
        };
        let location_kind = doc
            .get_i32("location_kind")
            .map_err(zvec_error)?
            .unwrap_or(2);
        let start = doc.get_i64("start").map_err(zvec_error)?.unwrap_or(0) as usize;
        let end = doc.get_i64("end").map_err(zvec_error)?.unwrap_or(0) as usize;
        let location = match location_kind {
            0 => FileLocation::Lines { start, end },
            1 => FileLocation::Page { number: start },
            _ => FileLocation::WholeFile,
        };
        let symbol_name = doc.get_string("symbol").map_err(zvec_error)?;
        let symbol_kind = doc.get_i32("symbol_kind").map_err(zvec_error)?.unwrap_or(8);
        let symbol = symbol_name
            .filter(|name| !name.is_empty())
            .map(|name| CodeSymbol {
                name,
                kind: decode_symbol_kind(symbol_kind),
            });
        Ok(RecallHit {
            segment: FileSegment {
                id,
                file_id: file_id.clone(),
                relative_path,
                kind,
                symbol,
                location,
                text: required_string(doc, "content")?,
            },
            file: self
                .manifest
                .files
                .get(&file_id)
                .cloned()
                .ok_or_else(|| storage_error("Zvec result references an unknown file"))?,
            score: doc.get_score(),
        })
    }
}

impl Storage for ZvecStorage {
    fn stats(&self) -> Result<StorageStats> {
        Ok(StorageStats {
            files: self.manifest.files.len(),
            segments: self.manifest.segment_ids.values().map(Vec::len).sum(),
        })
    }

    fn files(&self) -> Result<Vec<IndexedFile>> {
        Ok(self.manifest.files.values().cloned().collect())
    }

    fn replace_file(
        &mut self,
        file: IndexedFile,
        segments: Vec<FileSegment>,
        vectors: Vec<Vec<f32>>,
    ) -> Result<()> {
        self.replace_files(vec![(file, segments, vectors)])
    }

    fn replace_files(
        &mut self,
        files: Vec<(IndexedFile, Vec<FileSegment>, Vec<Vec<f32>>)>,
    ) -> Result<()> {
        for (_, segments, vectors) in &files {
            if segments.len() != vectors.len() {
                return Err(storage_error("segment and vector counts do not match"));
            }
            if vectors
                .iter()
                .any(|vector| vector.len() != self.manifest.dimension)
            {
                return Err(storage_error("vector dimension does not match the index"));
            }
        }

        let old_segment_ids: Vec<_> = files
            .iter()
            .flat_map(|(file, _, _)| {
                self.manifest
                    .segment_ids
                    .get(&file.id)
                    .into_iter()
                    .flatten()
                    .map(String::as_str)
            })
            .collect();
        if !old_segment_ids.is_empty() {
            self.collection
                .delete(&old_segment_ids)
                .map_err(zvec_error)?;
        }

        let segment_count: usize = files.iter().map(|(_, segments, _)| segments.len()).sum();
        let mut docs = Vec::with_capacity(segment_count);
        for (_, segments, vectors) in &files {
            for (segment, vector) in segments.iter().zip(vectors) {
                docs.push(segment_doc(segment, vector)?);
            }
        }
        if !docs.is_empty() {
            let refs: Vec<_> = docs.iter().collect();
            let write = self.collection.upsert(&refs).map_err(zvec_error)?;
            if write.error_count > 0 {
                return Err(storage_error(format!(
                    "Zvec rejected {} of {} segments",
                    write.error_count,
                    docs.len()
                )));
            }
        }
        for (file, segments, _) in files {
            let file_id = file.id.clone();
            self.manifest.files.insert(file_id.clone(), file);
            self.manifest.segment_ids.insert(
                file_id,
                segments.into_iter().map(|segment| segment.id).collect(),
            );
        }
        self.dirty = true;
        Ok(())
    }

    fn update_file(&mut self, file: IndexedFile) -> Result<()> {
        self.manifest.files.insert(file.id.clone(), file);
        self.dirty = true;
        Ok(())
    }

    fn remove_file(&mut self, id: &FileId) -> Result<()> {
        self.remove_files(std::slice::from_ref(id))
    }

    fn remove_files(&mut self, ids: &[FileId]) -> Result<()> {
        let segment_ids: Vec<_> = ids
            .iter()
            .flat_map(|id| {
                self.manifest
                    .segment_ids
                    .get(id)
                    .into_iter()
                    .flatten()
                    .map(String::as_str)
            })
            .collect();
        if !segment_ids.is_empty() {
            self.collection.delete(&segment_ids).map_err(zvec_error)?;
        }
        for id in ids {
            self.manifest.files.remove(id);
            self.manifest.segment_ids.remove(id);
        }
        self.dirty |= !ids.is_empty();
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        if self.dirty {
            self.collection.flush().map_err(zvec_error)?;
            self.save_manifest()?;
            self.dirty = false;
        }
        Ok(())
    }

    fn lexical_recall(&self, query: &str, limit: usize) -> Result<Vec<RecallHit>> {
        let mut fts = Fts::new().map_err(zvec_error)?;
        fts.set_match_string(query).map_err(zvec_error)?;
        let query = SearchQuery::fts("search_text", &fts, topk(limit)).map_err(zvec_error)?;
        self.collection
            .query(&query)
            .map_err(zvec_error)?
            .iter()
            .map(|doc| self.decode_doc(doc))
            .collect()
    }

    fn vector_recall(&self, vector: &[f32], limit: usize) -> Result<Vec<RecallHit>> {
        let query = SearchQuery::new("embedding", vector, topk(limit)).map_err(zvec_error)?;
        self.collection
            .query(&query)
            .map_err(zvec_error)?
            .iter()
            .map(|doc| self.decode_doc(doc))
            .collect()
    }
}

impl Drop for ZvecStorage {
    fn drop(&mut self) {
        if self.dirty {
            let _ = self.collection.flush();
            let _ = self.save_manifest();
        }
    }
}

fn ensure_initialized() -> Result<()> {
    static INITIALIZED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    INITIALIZED
        .get_or_init(|| zvec_rust::initialize(None).map_err(|error| error.to_string()))
        .clone()
        .map_err(storage_error)
}

fn required_string(doc: &Doc, field: &str) -> Result<String> {
    doc.get_string(field)
        .map_err(zvec_error)?
        .ok_or_else(|| storage_error(format!("Zvec result is missing {field}")))
}

fn decode_symbol_kind(value: i32) -> CodeSymbolKind {
    match value {
        0 => CodeSymbolKind::Function,
        1 => CodeSymbolKind::Method,
        2 => CodeSymbolKind::Type,
        3 => CodeSymbolKind::Interface,
        4 => CodeSymbolKind::Enum,
        5 => CodeSymbolKind::Trait,
        6 => CodeSymbolKind::Module,
        7 => CodeSymbolKind::Constant,
        _ => CodeSymbolKind::Other,
    }
}

fn topk(limit: usize) -> i32 {
    limit.min(i32::MAX as usize) as i32
}

fn zvec_error(error: zvec_rust::Error) -> EngineError {
    storage_error(error.to_string())
}

fn storage_error(message: impl Into<String>) -> EngineError {
    EngineError::Storage(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileFormat;

    #[test]
    fn persists_files_and_recalls_with_both_indexes() {
        let temp = tempfile::tempdir().unwrap();
        let embedding = EmbeddingSchema {
            model: "test".into(),
            dimension: 8,
            max_input_chars: 900,
        };
        let mut storage = ZvecStorage::open(temp.path(), &embedding).unwrap();
        let id = FileId::from_relative_path(Path::new("src/lib.rs"));
        let file = IndexedFile {
            id: id.clone(),
            relative_path: "src/lib.rs".into(),
            format: FileFormat::Rust,
            size: 10,
            modified_ms: 1,
            content_hash: "hash".into(),
        };
        let segment = FileSegment {
            id: "one".into(),
            file_id: id,
            relative_path: "src/lib.rs".into(),
            kind: SegmentKind::Code,
            symbol: Some(CodeSymbol {
                name: "parse_source".into(),
                kind: CodeSymbolKind::Function,
            }),
            location: FileLocation::Lines { start: 2, end: 4 },
            text: "parse source files".into(),
        };
        storage
            .replace_file(
                file,
                vec![segment],
                vec![vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]],
            )
            .unwrap();
        storage.commit().unwrap();
        assert_eq!(storage.lexical_recall("source", 5).unwrap().len(), 1);
        assert_eq!(
            storage
                .vector_recall(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 5)
                .unwrap()
                .len(),
            1
        );
        drop(storage);
        assert_eq!(
            ZvecStorage::open(temp.path(), &embedding)
                .unwrap()
                .files()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn model_changes_require_an_explicit_rebuild() {
        let temp = tempfile::tempdir().unwrap();
        let first = EmbeddingSchema {
            model: "first".into(),
            dimension: 8,
            max_input_chars: 900,
        };
        drop(ZvecStorage::open(temp.path(), &first).unwrap());
        let second = EmbeddingSchema {
            model: "second".into(),
            ..first
        };
        assert!(matches!(
            ZvecStorage::open(temp.path(), &second),
            Err(EngineError::Storage(_))
        ));
        assert!(ZvecStorage::recreate(temp.path(), &second).is_ok());
    }
}
