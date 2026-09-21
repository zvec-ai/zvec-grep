use super::super::zvec::{WRITE_BATCH, initialize};
use super::*;
use crate::domain::FileSnapshot;

fn file(id: u32, path: impl Into<PathBuf>) -> FileRecord {
    FileRecord {
        id: FileId::new(id),
        relative_path: crate::domain::SourcePath::new(path).expect("source path"),
        snapshot: FileSnapshot {
            size_bytes: 0,
            modified_epoch_ms: None,
            content_hash: None,
        },
        index_status: FileIndexStatus::NotIndexed,
    }
}

#[test]
fn source_file_projection_preserves_paths_and_directory_membership_for_all_statuses() {
    initialize().expect("initialize zvec");
    assert!(!files_schema().expect("file schema").has_field("formats"));
    let directories = [DirectoryId::new(0), DirectoryId::new(1)];
    let mut source = file(12, Path::new("src").join("nested").join("name.rs"));
    source.snapshot.content_hash = Some("fixture-hash".into());
    for status in [
        FileIndexStatus::NotIndexed,
        FileIndexStatus::Deleting,
        FileIndexStatus::Failed {
            error: "extractor unavailable".into(),
        },
        FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 0,
        },
    ] {
        source.index_status = status;
        let doc = encode_file_doc(&source, &directories).expect("encode source");
        assert_eq!(doc.get_pk(), Some("f12"));
        assert!(!doc.has_field("formats"));
        assert_eq!(
            string_field(&doc, "relative_path").expect("query path"),
            "src/nested/name.rs"
        );
        assert_eq!(
            string_field(&doc, "file_name").expect("file name"),
            "name.rs"
        );
        assert_eq!(
            doc.get_array_u32("ancestor_directory_ids")
                .expect("directory IDs"),
            Some(vec![0, 1])
        );
        assert_eq!(decode_file_doc(&doc).expect("decode source"), source);
    }
    let root_file = file(13, "main.rs");
    let root = encode_file_doc(&root_file, &[]).expect("root source");
    assert!(root.has_field("ancestor_directory_ids"));
}

#[test]
fn query_projections_read_all_paths_and_optional_times_without_decoding_payloads() {
    initialize().expect("initialize zvec");
    let temporary = tempfile::tempdir().expect("temporary storage");
    let storage_path = temporary.path().join("storage");
    std::fs::create_dir(&storage_path).expect("storage directory");
    let store = Files::open(&storage_path, false).expect("open storage");
    let mut expected = Vec::new();
    let mut expected_attributes = Vec::new();
    let mut docs = Vec::new();
    for index in 1..=WRITE_BATCH + 7 {
        let mut source = file(
            u32::try_from(index).expect("ID"),
            format!("file-{index}.rs"),
        );
        source.snapshot.modified_epoch_ms = match index % 3 {
            0 => None,
            1 => Some(0),
            _ => Some(u64::try_from(index).expect("modification time")),
        };
        let mut doc = encode_file_doc(&source, &[]).expect("encode source");
        doc.add_string("payload", "invalid full file payload")
            .expect("replace payload");
        docs.push(doc);
        expected_attributes.push(StoredFileAttributes::from(&source));
        expected.push((source.id, source.relative_path.into_path_buf()));
    }
    write_docs(&store.collection, &docs, "write source").expect("write projections");
    let mut actual = store.list_paths().expect("read lightweight paths");
    actual.sort_unstable();
    expected.sort_unstable();
    assert_eq!(actual, expected);
    let mut attributes = store.list_attributes().expect("read light file attributes");
    attributes.sort_unstable_by_key(|file| file.id);
    expected_attributes.sort_unstable_by_key(|file| file.id);
    assert_eq!(attributes, expected_attributes);
    assert!(store.list().is_err());
}

#[cfg(unix)]
#[test]
fn non_unicode_file_projection_keeps_native_path_without_a_lossy_query_value() {
    use std::os::unix::ffi::OsStringExt;

    initialize().expect("initialize zvec");
    let path = PathBuf::from(std::ffi::OsString::from_vec(b"src/\xff.rs".to_vec()));
    let source = file(9, path.clone());
    let doc = encode_file_doc(&source, &[DirectoryId::new(0)]).expect("encode non-Unicode source");
    assert!(!doc.has_field("relative_path"));
    // The native getter exposes an empty STRING as None, despite the field
    // being present and non-null. It is only a disabled query projection.
    assert!(doc.has_field("file_name"));
    assert!(!doc.is_field_null("file_name"));
    assert_eq!(
        doc.get_string("file_name")
            .expect("name")
            .unwrap_or_default(),
        ""
    );
    assert_eq!(
        decode_file_path_doc(&doc).expect("native path"),
        (source.id, path)
    );
    assert_eq!(decode_file_doc(&doc).expect("full source"), source);
}

#[test]
fn full_file_decode_rejects_a_path_projection_from_another_file() {
    initialize().expect("initialize zvec");
    let source = file(2, "first.rs");
    let mut doc = encode_file_doc(&source, &[]).expect("encode source");
    doc.add_string(
        "path",
        &encode_path(&crate::domain::SourcePath::new("second.rs").expect("source path"))
            .expect("path"),
    )
    .expect("replace path projection");
    assert!(decode_file_doc(&doc).is_err());
}

#[test]
fn full_range_ids_support_native_queries_membership_and_deletion() {
    initialize().expect("initialize zvec");
    let temporary = tempfile::tempdir().expect("temporary storage");
    let collection = open_collection(
        &temporary.path().join("files"),
        &files_schema().expect("file schema"),
        false,
    )
    .expect("file collection");
    let ids = [0, 1, i32::MAX as u32 + 1, u32::MAX - 1, u32::MAX];
    let sources: Vec<_> = ids
        .into_iter()
        .map(|id| file(id, format!("source-{id}.rs")))
        .collect();
    let docs: Vec<_> = sources
        .iter()
        .map(|source| encode_file_doc(source, &[]).expect("source document"))
        .collect();
    write_docs(&collection, &docs, "write source IDs").expect("write source IDs");
    collection.flush().expect("flush IDs");
    collection.optimize().expect("optimize full-range IDs");
    for source in &sources {
        for filter in [
            format!("file_id = {}", source.id),
            format!("file_id IN ({})", source.id),
        ] {
            let mut query = SearchQuery::scalar(10).expect("ID query");
            query.set_filter(&filter).expect("ID filter");
            let docs = collection.query(&query).expect("query ID");
            assert_eq!(docs.len(), 1, "{filter}");
            assert_eq!(decode_file_doc(&docs[0]).expect("decode source"), *source);
        }
    }
    collection
        .delete_by_filter(&format!("file_id = {}", u32::MAX))
        .expect("delete maximum ID");
    let query = SearchQuery::scalar(10).expect("remaining sources");
    let mut remaining: Vec<_> = collection
        .query(&query)
        .expect("query after deletion")
        .iter()
        .map(|doc| {
            decode_file_doc(doc)
                .expect("decode remaining source")
                .id
                .get()
        })
        .collect();
    remaining.sort_unstable();
    assert_eq!(remaining, ids[..ids.len() - 1]);
}

// TODO: re-enable on macOS once the zvec native binary registers the
// `array_take` Arrow compute kernel. See bug report: `fetch_with_options`
// silently returns a field-stripped Doc on macOS, so `mark_deleting`
// (fetch → mutate → upsert) persists a corrupted record and the follow-up
// `fetch` panics with "Field not found in document".
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "zvec macOS native missing array_take kernel; corrupts fetch→upsert path"
)]
fn deleting_preserves_file_projections_until_record_removal() {
    initialize().expect("initialize zvec");
    let root = tempfile::tempdir().expect("storage");
    let mut files = Files::open(root.path(), false).expect("files");
    let path = PathBuf::from("src/nested/main.rs");
    let id = files
        .resolve_ids(std::slice::from_ref(&path))
        .expect("identity")[0];
    let mut source = file(id.get(), path.clone());
    source.snapshot.modified_epoch_ms = Some(123);
    source.snapshot.content_hash = Some("content-hash".into());
    source.index_status = FileIndexStatus::Indexed {
        indexed_epoch_ms: 456,
        entity_count: 2,
    };
    files
        .put(&source, &[DirectoryId::new(3), DirectoryId::new(8)])
        .expect("source");
    files.mark_deleting(id).expect("mark deleting");
    let mut deleting = source.clone();
    deleting.index_status = FileIndexStatus::Deleting;
    assert_eq!(files.fetch(&[id]).expect("file").get(&id), Some(&deleting));
    let doc = fetch_map(&files.collection, &[file_key(id)])
        .expect("document")
        .remove(&file_key(id))
        .expect("file document");
    assert_eq!(
        doc.get_array_u32("ancestor_directory_ids")
            .expect("ancestors"),
        Some(vec![3, 8])
    );
    assert_eq!(string_field(&doc, "file_name").expect("name"), "main.rs");
    assert_eq!(
        string_field(&doc, "relative_path").expect("path"),
        "src/nested/main.rs"
    );
    assert_eq!(
        files
            .resolve_ids(std::slice::from_ref(&path))
            .expect("retained identity"),
        [id]
    );
    files.delete(id).expect("remove source");
    assert!(files.fetch(&[id]).expect("missing source").is_empty());
    assert_ne!(files.resolve_ids(&[path]).expect("new identity"), [id]);
}

#[test]
fn reader_opens_without_loading_the_file_identity_map() {
    initialize().expect("initialize zvec");
    let root = tempfile::tempdir().expect("storage");
    {
        let files = Files::open(root.path(), false).expect("files");
        let mut doc = encode_file_doc(&file(1, "main.rs"), &[]).expect("source");
        doc.add_string("path", "invalid native path")
            .expect("invalid projection");
        write_docs(&files.collection, &[doc], "write source").expect("source");
        files.flush().expect("flush");
    }
    let reader = Files::open(root.path(), true).expect("reader does not scan source identities");
    assert!(reader.list_paths().is_err());
    drop(reader);
    assert!(Files::open(root.path(), false).is_err());
}
