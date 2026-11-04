use super::*;
use crate::Record;
use std::io::Write;

#[test]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "small deterministic test values"
)]
fn lookup_matches_reconstructed_cosine() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let dim = 4;
    let chunks = 2;
    let pivots =
        rowmajor::Owned::from_fn(256, dim, |rc| ((rc.row + 1) * (rc.col + 2)) as f32 / 100.);
    let table =
        TransposedTable::from_parts(pivots.as_view(), ChunkOffsets::new(Box::new([0, 2, 4]))?)?;
    let pq = PqIndex {
        table,
        codes: vec![3, 7],
        count: 1,
    };
    for q in [[1., 2., -3., 4.], [0., 0., 0., 0.]] {
        let mut lut = rowmajor::Owned::from_element(chunks, 256, DotAndNorm::default());
        pq.table
            .process_into::<distances::Cosine, DotAndNorm>(&q, lut.as_view_mut());
        let v = [
            pivots.row(3)[0],
            pivots.row(3)[1],
            pivots.row(7)[2],
            pivots.row(7)[3],
        ];
        let dot: f64 = q
            .iter()
            .zip(v)
            .map(|(&a, b)| f64::from(a) * f64::from(b))
            .sum();
        let qn = q.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>().sqrt();
        let vn = v.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>().sqrt();
        let expected = if qn == 0. { 1. } else { 1. - dot / (qn * vn) };
        assert!((f64::from(pq.distance(1, &lut, qn as f32)?) - expected).abs() < 1e-6);
        assert!(pq.distance(2, &lut, qn as f32).is_err());
    }
    Ok(())
}

#[test]
fn reranking_uses_full_vectors_and_loader_checks_snapshot()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("index");
    let codes = dir.path().join("pq");
    let vectors = [[1., 0.], [0.8, 0.6], [0., 1.], [-1., 0.]];
    let writer =
        VectorIndex::open_with_precision(&path, 2, Distance::Cosine, false, VectorPrecision::Fp16)?;
    for (i, v) in vectors.iter().enumerate() {
        writer.upsert(&[Record {
            document_id: &format!("v{i}"),
            file_id: 1,
            vector: v,
        }])?;
    }
    let mut file = File::create(&codes)?;
    file.write_all(MAGIC)?;
    file.write_all(&2_u32.to_le_bytes())?;
    file.write_all(&1_u32.to_le_bytes())?;
    file.write_all(&4_u64.to_le_bytes())?;
    for i in 0..256 {
        for x in vectors.get(i).copied().unwrap_or([0., 0.]) {
            file.write_all(&x.to_le_bytes())?;
        }
    }
    // Deliberately swap codes for the best two vectors. Reranking must recover v0.
    file.write_all(&[1, 0, 2, 3])?;
    drop(file);
    assert!(PqIndex::load(&writer, &codes, 4).is_err());
    drop(writer);
    let index = VectorIndex::open_read_only(&path)?;
    let pq = PqIndex::load(&index, &codes, 4)?;
    let hits = index.search_with_pq(&pq, &[1., 0.], 1, 8, 4)?;
    assert_eq!(hits[0].document_id, "v0");
    assert!((hits[0].score - 1.).abs() < 1e-6);
    assert!(index.search_with_pq(&pq, &[1., 0.], 1, 2, 4).is_err());
    assert!(PqIndex::load(&index, &codes, 3).is_err());
    std::fs::OpenOptions::new()
        .write(true)
        .open(&codes)?
        .set_len(24)?;
    assert!(PqIndex::load(&index, &codes, 4).is_err());
    Ok(())
}
