//! PRIVATE local-cache benchmark. Explicit opt-in:
//! Set `ZG_PRIVATE_METIS_CACHE` to the real cache path and use an isolated HOME/TMPDIR;
//! `cargo test -p zg-engine private_real_cache_production_index -- --ignored --nocapture --test-threads=1`
//! Only aggregate results are printed. Cache keys, vectors, and source text are not printed.
//! Shape labels are inferred, not proof of purpose; identities and text are synthetic.
//! Temporary indexes contain sensitive derived data until removed at normal exit.
//! PSS is sequential in the same process, not isolated or directly comparable.

use super::*;
use crate::domain::{Content, FileIndexStatus, FileSnapshot, Range};
use rusqlite::{Connection, OpenFlags};
use std::{collections::HashSet, fs, time::Instant};

const DIM: usize = 1024;
const MAX_ROWS: usize = 200_000;
const MAX_DOCS: usize = 1_024;
const MAX_QUERIES: usize = 3;
const MODEL: &str = "fixture/fixture";

fn sample() -> (Vec<Vec<f32>>, Vec<Vec<f32>>, usize) {
    let path = std::path::PathBuf::from(
        std::env::var_os("ZG_PRIVATE_METIS_CACHE").expect("explicit private cache path"),
    );
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("open cache read-only");
    db.execute_batch("BEGIN").expect("read snapshot");
    let mut statement = db
        .prepare("SELECT embedding FROM metis_embeddings ORDER BY cache_key LIMIT ?1")
        .expect("cache schema");
    let mut rows = statement
        .query([i64::try_from(MAX_ROWS).expect("bounded row limit")])
        .expect("read cache");
    let (mut docs, mut queries, mut scanned) = (Vec::new(), Vec::new(), 0);
    while let Some(row) = rows.next().expect("cache row") {
        scanned += 1;
        let blob: Vec<u8> = row.get(0).expect("cache blob");
        if blob.len() != DIM * 4 {
            continue;
        }
        let vector: Vec<f32> = blob
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        if vector.iter().any(|v| !v.is_finite()) {
            continue;
        }
        let signs = vector
            .iter()
            .filter(|v| v.abs().to_bits() == 0.03125_f32.to_bits())
            .count();
        if signs == DIM && docs.len() < MAX_DOCS {
            docs.push(vector);
        } else if signs < DIM / 10
            && vector.iter().filter(|v| **v != 0.0).count() >= DIM * 9 / 10
            && vector.iter().any(|v| *v > 0.0)
            && vector.iter().any(|v| *v < 0.0)
            && queries.len() < MAX_QUERIES
        {
            queries.push(vector);
        }
        if docs.len() == MAX_DOCS && queries.len() == MAX_QUERIES {
            break;
        }
    }
    (docs, queries, scanned)
}

fn model(quantization: VectorIndexQuantization) -> EmbeddingModelInfo {
    EmbeddingModelInfo::new(
        crate::domain::model::ModelInfo::new(
            "fixture",
            "fixture",
            [crate::domain::ContentKind::Text],
            32,
        )
        .expect("fixture model"),
        DIM,
        EmbeddingMetric::Cosine,
        None,
        None,
    )
    .expect("fixture embedding model")
    .with_vector_index_quantization(quantization)
}

fn fixture(index: usize, vector: &[f32]) -> (FileRecord, Entity, IndexedFragment) {
    let file_id = FileId::new(u32::try_from(index + 1).expect("bounded fixture id"));
    let file = FileRecord::new(
        file_id,
        SourcePath::new(format!("fixture/{index}.txt")).expect("fixture path"),
        FileSnapshot {
            size_bytes: 7,
            modified_epoch_ms: None,
            content_hash: Some("fixture-hash".into()),
        },
        FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 1,
        },
    )
    .expect("fixture file");
    let content = Content::Text("fixture".into());
    let id = EntityId::new(file_id, &content, Range::Full).expect("fixture entity id");
    let fragment_id = FragmentId::new(&id, 0);
    let entity = Entity::new(
        id.clone(),
        file_id,
        Range::Full,
        content,
        None,
        vec![EntityFragment {
            id: fragment_id.clone(),
            range: Range::Full,
        }],
    )
    .expect("fixture entity");
    let entry = IndexedFragment {
        entity_id: id,
        fragment_id,
        model: MODEL.into(),
        vector: vector.to_vec(),
        fts_text: "fixture".into(),
    };
    (file, entity, entry)
}

fn bytes(path: &Path) -> u64 {
    fs::read_dir(path)
        .expect("read temporary directory")
        .map(|e| {
            let e = e.expect("temporary directory entry");
            let m = e.metadata().expect("temporary entry metadata");
            if m.is_dir() {
                bytes(&e.path())
            } else {
                m.len()
            }
        })
        .sum()
}

fn pss_kib() -> Option<u64> {
    let smaps = fs::read_to_string("/proc/self/smaps_rollup").ok()?;
    smaps.lines().find_map(|line| {
        line.strip_prefix("Pss:")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

fn exact(docs: &[Vec<f32>], queries: &[Vec<f32>]) -> Vec<HashSet<String>> {
    queries
        .iter()
        .map(|q| {
            let qnorm = q.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
            let mut scores: Vec<_> = docs
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let dot: f64 = d
                        .iter()
                        .zip(q)
                        .map(|(a, b)| f64::from(*a) * f64::from(*b))
                        .sum();
                    let norm = d.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
                    (i, dot / (norm * qnorm))
                })
                .collect();
            scores.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            scores
                .into_iter()
                .take(10)
                .map(|(i, _)| fixture(i, &docs[i]).2.fragment_id.as_str().to_owned())
                .collect()
        })
        .collect()
}

fn run_variant(
    label: &str,
    quantization: VectorIndexQuantization,
    docs: &[Vec<f32>],
    queries: &[Vec<f32>],
    truth: &[HashSet<String>],
) {
    let dir = tempfile::Builder::new()
        .prefix("zg-private-cache-bench-")
        .tempdir()
        .expect("private temporary directory");
    let store =
        Fragments::open(dir.path(), &[model(quantization)], false).expect("temporary fragments");
    let start = Instant::now();
    for (i, vector) in docs.iter().enumerate() {
        let (file, entity, entry) = fixture(i, vector);
        let projections =
            Fragments::prepare(&file, &[entity], &[entry], &[], store.embedding_models())
                .expect("prepare fixture");
        store.write(&projections).expect("write fixture");
    }
    store.optimize().expect("build production indexes");
    store.flush().expect("flush");
    assert!(
        !store.needs_optimize().expect("inspect index completeness"),
        "temporary index is not fully optimized"
    );
    let build_ms = start.elapsed().as_secs_f64() * 1000.0;
    let mut total_ms = 0.0;
    let mut overlap = 0;
    for (q, expected) in queries.iter().zip(truth) {
        let start = Instant::now();
        let hits = store
            .search_vector(MODEL, q, 10, None)
            .expect("production vector search");
        total_ms += start.elapsed().as_secs_f64() * 1000.0;
        overlap += hits
            .iter()
            .filter(|hit| expected.contains(&hit.document_id))
            .count();
        assert_eq!(hits.len(), 10.min(docs.len()));
    }
    println!(
        "{label}: build_ms={build_ms:.1} mean_query_ms={:.3} overlap={}/{} disk_bytes={} pss_kib={:?}",
        total_ms / f64::from(u32::try_from(queries.len()).expect("bounded query count")),
        overlap,
        queries.len() * 10.min(docs.len()),
        bytes(dir.path()),
        pss_kib()
    );
    drop(store);
}

#[test]
#[ignore = "PRIVATE local cache benchmark; explicitly opt in"]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn private_real_cache_production_index() {
    let (docs, queries, scanned) = sample();
    assert!(docs.len() >= 10, "not enough document-shaped cache rows");
    assert!(
        queries.len() >= 3,
        "not enough query-shaped cache rows within scan bound"
    );
    println!(
        "PRIVATE aggregate: scanned={scanned} docs={} queries={} dim={DIM}; labels inferred, cache expiry ignored",
        docs.len(),
        queries.len()
    );
    if docs.len() <= 1000 {
        println!("LIMITATION: fewer than 1001 document-shaped rows available within scan bound");
    }
    let truth = exact(&docs, &queries);
    super::super::zvec::initialize().expect("initialize zvec");
    run_variant(
        "FP16/HNSW",
        VectorIndexQuantization::Fp16,
        &docs,
        &queries,
        &truth,
    );
    run_variant(
        "binary-sign scan",
        VectorIndexQuantization::BinarySign,
        &docs,
        &queries,
        &truth,
    );
}
