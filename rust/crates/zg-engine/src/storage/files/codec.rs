use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use super::super::{
    path::PathRecord,
    record::{decode, encode, invalid_record},
};
use crate::{
    EngineResult,
    domain::{FileId, FileIndexStatus, FileRecord, FileSnapshot, SourcePath},
};

pub(super) fn encode_file(file: &FileRecord) -> EngineResult<String> {
    file.validate()?;
    encode(FilePayload::from_file(file)?, "source file")
}

pub(super) fn decode_file(json: &str) -> EngineResult<FileRecord> {
    let record: FilePayload<'static> = decode(json, "source file")?;
    record
        .into_file()
        .map_err(|error| invalid_record("source file", &error))
}

#[derive(Serialize, Deserialize)]
struct FilePayload<'a> {
    id: u32,
    relative_path: PathRecord,
    snapshot: SnapshotRecord<'a>,
    index_status: IndexStatusRecord<'a>,
}

#[derive(Serialize, Deserialize)]
struct SnapshotRecord<'a> {
    size_bytes: u64,
    modified_epoch_ms: Option<u64>,
    content_hash: Option<Cow<'a, str>>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum IndexStatusRecord<'a> {
    NotIndexed {},
    Indexed {
        indexed_epoch_ms: u64,
        entity_count: u64,
    },
    Deleting {},
    Failed {
        error: Cow<'a, str>,
    },
}

impl<'a> From<&'a FileIndexStatus> for IndexStatusRecord<'a> {
    fn from(status: &'a FileIndexStatus) -> Self {
        match status {
            FileIndexStatus::NotIndexed => Self::NotIndexed {},
            FileIndexStatus::Indexed {
                indexed_epoch_ms,
                entity_count,
            } => Self::Indexed {
                indexed_epoch_ms: *indexed_epoch_ms,
                entity_count: *entity_count,
            },
            FileIndexStatus::Deleting => Self::Deleting {},
            FileIndexStatus::Failed { error } => Self::Failed {
                error: error.as_str().into(),
            },
        }
    }
}

impl From<IndexStatusRecord<'_>> for FileIndexStatus {
    fn from(status: IndexStatusRecord<'_>) -> Self {
        match status {
            IndexStatusRecord::NotIndexed {} => Self::NotIndexed,
            IndexStatusRecord::Indexed {
                indexed_epoch_ms,
                entity_count,
            } => Self::Indexed {
                indexed_epoch_ms,
                entity_count,
            },
            IndexStatusRecord::Deleting {} => Self::Deleting,
            IndexStatusRecord::Failed { error } => Self::Failed {
                error: error.into_owned(),
            },
        }
    }
}

impl<'a> FilePayload<'a> {
    fn from_file(file: &'a FileRecord) -> EngineResult<Self> {
        Ok(Self {
            index_status: (&file.index_status).into(),
            id: file.id.get(),
            relative_path: PathRecord::from_path(&file.relative_path)?,
            snapshot: SnapshotRecord {
                size_bytes: file.snapshot.size_bytes,
                modified_epoch_ms: file.snapshot.modified_epoch_ms,
                content_hash: file.snapshot.content_hash.as_deref().map(Cow::Borrowed),
            },
        })
    }

    fn into_file(self) -> EngineResult<FileRecord> {
        let file = FileRecord {
            index_status: self.index_status.into(),
            id: FileId::new(self.id),
            relative_path: SourcePath::new(self.relative_path.into_path()?)?,
            snapshot: FileSnapshot {
                size_bytes: self.snapshot.size_bytes,
                modified_epoch_ms: self.snapshot.modified_epoch_ms,
                content_hash: self.snapshot.content_hash.map(Cow::into_owned),
            },
        };
        file.validate()?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn file() -> FileRecord {
        FileRecord {
            index_status: FileIndexStatus::NotIndexed,
            id: FileId::new(1),
            relative_path: SourcePath::new("nested/tsconfig.json").expect("source path"),
            snapshot: FileSnapshot {
                size_bytes: 123,
                modified_epoch_ms: Some(456),
                content_hash: Some("content-hash".into()),
            },
        }
    }

    #[test]
    fn source_records_preserve_snapshots_and_native_paths_without_formats() {
        let mut source = file();
        let encoded = encode_file(&source).expect("encode source");
        let record: Value = serde_json::from_str(&encoded).expect("JSON record");
        assert_eq!(
            record,
            json!({
                "id": 1, "index_status": {"kind": "not_indexed"},
                "relative_path": {"encoding": "utf8", "value": "nested/tsconfig.json"},
                "snapshot": {"size_bytes": 123, "modified_epoch_ms": 456, "content_hash": "content-hash"},
            })
        );
        assert_eq!(decode_file(&encoded).expect("decode source"), source);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            source.relative_path =
                SourcePath::new(std::ffi::OsString::from_vec(b"file-\xff.rs".to_vec()))
                    .expect("source path");
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            source.relative_path = SourcePath::new(std::ffi::OsString::from_wide(&[
                0x0066, 0xd800, 0x002e, 0x0072, 0x0073,
            ]))
            .expect("source path");
        }
        assert_eq!(
            decode_file(&encode_file(&source).expect("encode native path"))
                .expect("decode native path"),
            source
        );
    }

    #[test]
    fn source_identities_round_trip_the_full_u32_range() {
        for id in [0, u32::MAX] {
            let mut source = file();
            source.id = FileId::new(id);
            assert_eq!(
                decode_file(&encode_file(&source).expect("encode source")).expect("decode source"),
                source
            );
        }
    }

    #[test]
    fn stored_file_identities_reject_values_outside_u32() {
        let mut record: Value =
            serde_json::from_str(&encode_file(&file()).expect("source")).expect("JSON");
        for invalid in [json!(-1), json!(u64::from(u32::MAX) + 1), json!(1e20)] {
            record["id"] = invalid;
            assert!(decode_file(&record.to_string()).is_err());
        }
    }

    #[test]
    fn file_status_round_trips_and_rejects_invalid_persisted_states() {
        let mut file = file();
        for status in [
            FileIndexStatus::NotIndexed,
            FileIndexStatus::Indexed {
                indexed_epoch_ms: 789,
                entity_count: 0,
            },
            FileIndexStatus::Indexed {
                indexed_epoch_ms: 789,
                entity_count: u64::MAX,
            },
            FileIndexStatus::Deleting,
            FileIndexStatus::Failed {
                error: "extraction failed".into(),
            },
        ] {
            file.index_status = status;
            assert_eq!(
                decode_file(&encode_file(&file).expect("encode status")).expect("decode status"),
                file
            );
        }
        let mut record: Value =
            serde_json::from_str(&encode_file(&file).expect("valid file")).expect("JSON");
        for invalid in [
            json!({"kind":"indexed", "indexed_epoch_ms":1, "entity_count":0, "error":"failure"}),
            json!({"kind":"indexed", "indexed_epoch_ms":null, "entity_count":0}),
            json!({"kind":"indexed", "indexed_epoch_ms":1, "entity_count":-1}),
            json!({"kind":"not_indexed", "entity_count":1}),
            json!({"kind":"running"}),
            Value::Null,
        ] {
            record["index_status"] = invalid;
            assert!(decode_file(&record.to_string()).is_err());
        }
        record["index_status"] = json!({"kind":"indexed", "indexed_epoch_ms":1, "entity_count":0});
        record["snapshot"]["content_hash"] = Value::Null;
        assert!(decode_file(&record.to_string()).is_err());
    }
}
