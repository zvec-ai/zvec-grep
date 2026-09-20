use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    EngineError, EngineResult,
    domain::{FileId, FileIndexStatus, FileRecord},
    utils::{atomic_write, sync_directory},
};

use super::codec;

pub(super) const NAME: &str = "pending.json";
const VERSION: u32 = 2;

#[cfg(test)]
thread_local! {
    pub(super) static WRITE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PendingChange {
    Reindex(FileRecord),
    Delete(FileId),
}

impl PendingChange {
    pub(super) fn reindex(file: &FileRecord) -> Self {
        let mut file = file.clone();
        file.index_status = FileIndexStatus::NotIndexed;
        Self::Reindex(file)
    }

    pub(super) fn file_id(&self) -> &FileId {
        match self {
            Self::Reindex(source) => &source.id,
            Self::Delete(file_id) => file_id,
        }
    }
}

pub(super) type PendingChanges = BTreeMap<FileId, PendingChange>;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PendingRecord {
    version: u32,
    files: Vec<ChangeRecord>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ChangeRecord {
    Reindex { source: String },
    Delete { file_id: u32 },
}

pub(super) fn write(storage_path: &Path, changes: &PendingChanges) -> EngineResult<()> {
    #[cfg(test)]
    WRITE_COUNT.with(|count| count.set(count.get() + 1));
    if changes.is_empty() {
        return Err(invalid("pending batch must not be empty"));
    }
    let files = changes
        .iter()
        .map(|(key, change)| {
            if key != change.file_id() {
                return Err(invalid("pending key differs from its source file ID"));
            }
            match change {
                PendingChange::Reindex(source) => Ok(ChangeRecord::Reindex {
                    source: codec::encode_file(source)?,
                }),
                PendingChange::Delete(file_id) => Ok(ChangeRecord::Delete {
                    file_id: file_id.get(),
                }),
            }
        })
        .collect::<EngineResult<Vec<_>>>()?;
    let bytes = serde_json::to_vec(&PendingRecord {
        version: VERSION,
        files,
    })
    .map_err(|error| invalid(format!("cannot encode pending batch: {error}")))?;
    atomic_write(&storage_path.join(NAME), &bytes)
}

pub(super) fn read(storage_path: &Path) -> EngineResult<PendingChanges> {
    let path = storage_path.join(NAME);
    let bytes = fs::read(&path).map_err(|error| {
        EngineError::from_io(
            format!(
                "cannot read pending storage record {}: {error}",
                path.display()
            ),
            &error,
        )
    })?;
    decode(&bytes).map_err(|error| {
        invalid(format!(
            "invalid pending storage record {}: {}",
            path.display(),
            error.message()
        ))
    })
}

pub(super) fn clear(storage_path: &Path) -> EngineResult<()> {
    let path = storage_path.join(NAME);
    fs::remove_file(&path).map_err(|error| {
        EngineError::from_io(
            format!(
                "cannot remove pending storage record {}: {error}",
                path.display()
            ),
            &error,
        )
    })?;
    sync_directory(storage_path)
}

fn decode(bytes: &[u8]) -> EngineResult<PendingChanges> {
    let record: PendingRecord = serde_json::from_slice(bytes)
        .map_err(|error| invalid(format!("cannot decode pending batch: {error}")))?;
    if record.version != VERSION {
        return Err(invalid(format!(
            "unsupported pending record version {}; expected {VERSION}",
            record.version
        )));
    }
    if record.files.is_empty() {
        return Err(invalid("pending batch must not be empty"));
    }
    let mut changes = PendingChanges::new();
    for file in record.files {
        let change = match file {
            ChangeRecord::Reindex { source } => {
                PendingChange::Reindex(codec::decode_file(&source)?)
            }
            ChangeRecord::Delete { file_id } => PendingChange::Delete(FileId::new(file_id)),
        };
        let key = *change.file_id();
        if changes.insert(key, change).is_some() {
            return Err(invalid("pending batch contains duplicate source file IDs"));
        }
    }
    Ok(changes)
}

fn invalid(message: impl Into<String>) -> EngineError {
    EngineError::storage_failure(message)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::domain::FileSnapshot;

    fn source() -> FileRecord {
        FileRecord {
            index_status: crate::domain::FileIndexStatus::NotIndexed,
            id: FileId::new(0),
            relative_path: crate::domain::SourcePath::new("nested/source.rs").expect("source path"),
            snapshot: FileSnapshot {
                size_bytes: 42,
                modified_epoch_ms: Some(123),
                content_hash: Some("source-hash".to_owned()),
            },
        }
    }

    fn decode_value(value: &Value) -> EngineResult<PendingChanges> {
        decode(&serde_json::to_vec(value).expect("encode test record"))
    }

    #[test]
    fn reindex_intent_never_claims_a_completed_index() {
        let mut file = source();
        file.index_status = FileIndexStatus::Indexed {
            indexed_epoch_ms: 7,
            entity_count: 3,
        };
        let PendingChange::Reindex(pending) = PendingChange::reindex(&file) else {
            panic!("reindex intention");
        };
        assert_eq!(pending.index_status, FileIndexStatus::NotIndexed);
        assert_eq!(pending.snapshot, file.snapshot);
        assert_eq!(pending.id, file.id);
        assert!(
            file.index_status.is_indexed(),
            "input record remains intact"
        );
    }

    #[test]
    fn persists_only_source_metadata_and_deletion_intents() {
        let directory = tempfile::tempdir().expect("pending directory");
        let source = source();
        let deletion = FileId::new(u32::MAX);
        let changes = PendingChanges::from([
            (source.id, PendingChange::Reindex(source)),
            (deletion, PendingChange::Delete(deletion)),
        ]);

        write(directory.path(), &changes).expect("write pending batch");
        let record: Value = serde_json::from_slice(
            &fs::read(directory.path().join(NAME)).expect("read pending JSON"),
        )
        .expect("parse pending JSON");
        assert_eq!(record["version"], VERSION);
        assert_eq!(record["files"].as_array().expect("pending files").len(), 2);
        for change in record["files"].as_array().expect("pending files") {
            assert_eq!(change.as_object().expect("pending change").len(), 2);
            if let Some(source) = change["source"].as_str() {
                let payload: Value = serde_json::from_str(source).expect("source payload");
                assert!(payload["value"].get("formats").is_none());
            }
        }
        assert_eq!(read(directory.path()).expect("read pending batch"), changes);
        clear(directory.path()).expect("clear pending batch");
        assert!(!directory.path().join(NAME).exists());
    }

    #[test]
    fn rejects_invalid_batches_before_returning_any_changes() {
        let encoded = codec::encode_file(&source()).expect("encode source");
        let mut invalid_source: Value = serde_json::from_str(&encoded).expect("source record");
        invalid_source["value"]["relative_path"]["value"] = json!("../outside.rs");
        let mut invalid_id: Value = serde_json::from_str(&encoded).expect("source record");
        invalid_id["value"]["id"] = json!(-1);
        let reindex = json!({"kind": "reindex", "source": encoded});
        for record in [
            json!({"version": VERSION, "files": []}),
            json!({"version": VERSION + 1, "files": [reindex.clone()]}),
            json!({"version": VERSION, "files": [reindex.clone()], "unexpected": true}),
            json!({"version": VERSION, "files": [reindex.clone(), reindex.clone()]}),
            json!({"version": VERSION, "files": [reindex.clone(), {"kind": "delete", "file_id": 0}]}),
            json!({"version": VERSION, "files": [reindex.clone(), {"kind": "delete", "file_id": -1}]}),
            json!({"version": VERSION, "files": [reindex.clone(), {"kind": "delete", "file_id": u64::from(u32::MAX) + 1}]}),
            json!({"version": VERSION, "files": [reindex.clone(), {"kind": "reindex", "source": "broken"}]}),
            json!({"version": VERSION, "files": [{"kind": "reindex", "source": invalid_source.to_string()}]}),
            json!({"version": VERSION, "files": [{"kind": "reindex", "source": invalid_id.to_string()}]}),
            json!({"version": VERSION, "files": [{"kind": "delete", "file_id": 2, "entries": []}]}),
        ] {
            assert!(decode_value(&record).is_err(), "accepted {record}");
        }
        for bytes in [b"not JSON".as_slice(), br#"{"version":1,"files":["#] {
            assert!(decode(bytes).is_err());
        }
    }

    #[test]
    fn rejected_writes_preserve_the_existing_pending_batch() {
        let directory = tempfile::tempdir().expect("pending directory");
        let source = source();
        let changes = PendingChanges::from([(source.id, PendingChange::Reindex(source.clone()))]);
        write(directory.path(), &changes).expect("write initial pending batch");
        let mut invalid_source = source.clone();
        invalid_source.index_status = FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 1,
        };
        invalid_source.snapshot.content_hash = None;
        for invalid in [
            PendingChanges::new(),
            PendingChanges::from([(FileId::new(2), PendingChange::Reindex(source))]),
            PendingChanges::from([(invalid_source.id, PendingChange::Reindex(invalid_source))]),
        ] {
            assert!(write(directory.path(), &invalid).is_err());
            assert_eq!(
                read(directory.path()).expect("original pending batch"),
                changes
            );
        }
    }
}
