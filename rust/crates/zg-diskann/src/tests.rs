use super::*;

#[test]
fn persists_metrics_replacements_deletions_and_read_only_queries() {
    for metric in [Distance::Cosine, Distance::DotProduct, Distance::Euclidean] {
        let home = tempfile::tempdir().expect("temporary storage");
        let path = home.path().join("vectors.sqlite");
        let index = VectorIndex::open(&path, 3, metric, false).expect("create");
        assert!(
            index
                .search(&[1.0, 0.0, 0.0], 10, None)
                .expect("empty")
                .is_empty()
        );
        index
            .upsert(&[
                Record {
                    document_id: "a",
                    file_id: 1,
                    vector: &[1.0, 0.0, 0.0],
                },
                Record {
                    document_id: "b",
                    file_id: 2,
                    vector: &[0.0, 1.0, 0.0],
                },
            ])
            .expect("insert");
        assert_eq!(
            index.search(&[1.0, 0.0, 0.0], 1, None).expect("search")[0].document_id,
            "a"
        );
        index
            .upsert(&[Record {
                document_id: "a",
                file_id: 1,
                vector: &[-1.0, 0.0, 0.0],
            }])
            .expect("replace");
        assert_eq!(
            index
                .search(&[1.0, 0.0, 0.0], 1, None)
                .expect("replacement")[0]
                .document_id,
            "b"
        );
        index.delete_file(2).expect("delete");
        index.delete_file(2).expect("delete twice");
        drop(index);
        let bytes = std::fs::read(&path).expect("database bytes");
        let index = VectorIndex::open(&path, 3, metric, true).expect("reopen");
        let hits = index
            .search(&[-1.0, 0.0, 0.0], 10, None)
            .expect("persisted search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].document_id, "a");
        let expected = if metric == Distance::Euclidean {
            0.0
        } else {
            1.0
        };
        assert!((hits[0].score - expected).abs() < 1e-6);
        assert!(index.delete_file(1).is_err());
        assert!(index.upsert(&[]).is_err());
        drop(index);
        assert_eq!(std::fs::read(path).expect("unchanged bytes"), bytes);
    }
}

#[test]
fn restrictive_filter_finds_distant_match_beyond_unfiltered_top_k() {
    let home = tempfile::tempdir().expect("temporary storage");
    let index = VectorIndex::open(
        &home.path().join("vectors.sqlite"),
        2,
        Distance::Euclidean,
        false,
    )
    .expect("index");
    let names = (0..180).map(|i| format!("doc-{i}")).collect::<Vec<_>>();
    let vectors = (0_u16..180)
        .map(|i| [f32::from(i), 1.0])
        .collect::<Vec<_>>();
    let records = names
        .iter()
        .zip(&vectors)
        .map(|(name, vector)| Record {
            document_id: name,
            file_id: 1,
            vector,
        })
        .collect::<Vec<_>>();
    index.upsert(&records).expect("insert corpus");
    let allowed = HashSet::from(["doc-179".to_owned()]);
    let hits = index
        .search(&[0.0, 1.0], 1, Some(&allowed))
        .expect("filtered search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].document_id, "doc-179");
    assert!(
        index
            .search(&[0.0, 1.0], 1, Some(&HashSet::new()))
            .expect("empty filter")
            .is_empty()
    );
}

#[test]
fn failed_graph_write_rolls_back_vectors_and_edges() {
    let home = tempfile::tempdir().expect("temporary storage");
    let path = home.path().join("vectors.sqlite");
    let index = VectorIndex::open(&path, 2, Distance::Euclidean, false).expect("index");
    index
        .upsert(&[Record {
            document_id: "original",
            file_id: 1,
            vector: &[1.0, 0.0],
        }])
        .expect("original");
    let connection = Connection::open(&path).expect("database");
    connection.execute_batch("CREATE TRIGGER fail_edges BEFORE UPDATE OF neighbors ON nodes BEGIN SELECT RAISE(ABORT, 'injected edge failure'); END;").expect("fault injection");
    assert!(
        index
            .upsert(&[Record {
                document_id: "new",
                file_id: 2,
                vector: &[0.0, 1.0]
            }])
            .is_err()
    );
    let count: u32 = connection
        .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
        .expect("node count");
    assert_eq!(count, 2); // Start node and original node only.
    connection
        .execute_batch("DROP TRIGGER fail_edges")
        .expect("remove fault");
    let hits = index
        .search(&[0.0, 1.0], 10, None)
        .expect("original searchable");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].document_id, "original");
}

#[test]
fn rejects_invalid_vectors_and_incompatible_storage() {
    let home = tempfile::tempdir().expect("temporary storage");
    let path = home.path().join("vectors.sqlite");
    assert!(VectorIndex::open(&path, 2, Distance::Cosine, true).is_err());
    let index = VectorIndex::open(&path, 2, Distance::Cosine, false).expect("index");
    assert!(
        index
            .search_with_list_size(&[0.0, 0.0], 1, None, 0)
            .is_err()
    );
    for vector in [&[1.0][..], &[f32::NAN, 1.0], &[f32::INFINITY, 0.0]] {
        assert!(
            index
                .upsert(&[Record {
                    document_id: "bad",
                    file_id: 1,
                    vector
                }])
                .is_err()
        );
        assert!(index.search(vector, 10, None).is_err());
    }
    index
        .upsert(&[Record {
            document_id: "zero",
            file_id: 1,
            vector: &[0.0, 0.0],
        }])
        .expect("zero vector");
    assert!(
        index.search(&[0.0, 0.0], 1, None).expect("zero query")[0]
            .score
            .is_finite()
    );
    drop(index);
    assert!(VectorIndex::open(&path, 3, Distance::Cosine, true).is_err());
    assert!(VectorIndex::open(&path, 2, Distance::Euclidean, true).is_err());
}

#[test]
fn reused_reader_refreshes_liveness_and_filters_after_committed_writes() {
    let home = tempfile::tempdir().expect("temporary storage");
    let path = home.path().join("vectors.sqlite");
    let writer = VectorIndex::open(&path, 2, Distance::Cosine, false).expect("writer");
    writer
        .upsert(&[
            Record {
                document_id: "a",
                file_id: 1,
                vector: &[1.0, 0.0],
            },
            Record {
                document_id: "b",
                file_id: 2,
                vector: &[0.0, 1.0],
            },
        ])
        .expect("insert");
    let reader = VectorIndex::open_read_only(&path).expect("reader");
    assert_eq!(
        reader.search(&[1.0, 0.0], 1, None).expect("warm query")[0].document_id,
        "a"
    );
    writer.delete_file(1).expect("delete");
    let allowed = HashSet::from(["a".to_owned()]);
    assert!(
        reader
            .search(&[1.0, 0.0], 1, Some(&allowed))
            .expect("deleted filter")
            .is_empty()
    );
    assert_eq!(
        reader.search(&[1.0, 0.0], 1, None).expect("refresh query")[0].document_id,
        "b"
    );
    writer
        .upsert(&[Record {
            document_id: "a",
            file_id: 1,
            vector: &[1.0, 0.0],
        }])
        .expect("replace");
    assert_eq!(
        reader
            .search(&[1.0, 0.0], 1, Some(&allowed))
            .expect("replaced filter")[0]
            .document_id,
        "a"
    );
}
