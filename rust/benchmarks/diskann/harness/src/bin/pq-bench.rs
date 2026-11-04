#![allow(
    clippy::chunks_exact_to_as_chunks,
    reason = "preserve the byte-decoding loop used for the recorded measurements"
)]

use serde_json::json;
use std::{
    fs::File,
    io::{BufReader, Read},
    path::Path,
    time::Instant,
};
use zg_diskann::{VectorIndex, experimental_pq::PqIndex};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    assert!(
        a.len() == 6 || a.len() == 8,
        "full|pq index count L queries [codes rerank]"
    );
    let count: usize = a[3].parse().unwrap();
    let effort: usize = a[4].parse().unwrap();
    assert!(matches!(a[1].as_str(), "full" | "pq"));
    assert_eq!(a.len(), if a[1] == "pq" { 8 } else { 6 });
    // Match the original harness's process initialization in both variants.
    zvec_rust::initialize(None).unwrap();
    let index = VectorIndex::open_read_only(Path::new(&a[2])).unwrap();
    let pq = if a[1] == "pq" {
        Some(PqIndex::load(&index, Path::new(&a[6]), count).unwrap())
    } else {
        None
    };
    let rerank = if pq.is_some() {
        a[7].parse().unwrap()
    } else {
        0
    };
    let file = File::open(&a[5]).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 220 * 768 * 4);
    let mut reader = BufReader::new(file);
    let mut ids = Vec::with_capacity(200);
    let mut latencies = Vec::with_capacity(200);
    let mut batch_started = None;
    for i in 0..220 {
        if i == 20 {
            batch_started = Some(Instant::now());
        }
        let mut bytes = [0_u8; 768 * 4];
        reader.read_exact(&mut bytes).unwrap();
        let mut query: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        let started = Instant::now();
        for x in &mut query {
            *x = half::f16::from_f32(*x).to_f32();
        }
        let hits = if let Some(pq) = &pq {
            index
                .search_with_pq(pq, &query, 10, effort, rerank)
                .unwrap()
        } else {
            index
                .search_with_list_size(&query, 10, None, effort)
                .unwrap()
        };
        let found: Vec<_> = hits.into_iter().map(|h| h.document_id).collect();
        let ms = started.elapsed().as_secs_f64() * 1000.;
        assert_eq!(found.len(), 10);
        if i >= 20 {
            ids.push(found);
            latencies.push(ms);
        }
    }
    let batch_ms = batch_started.unwrap().elapsed().as_secs_f64() * 1000.;
    latencies.sort_by(f64::total_cmp);
    println!(
        "{}",
        json!({"stage":"queried","engine":if pq.is_some(){"diskann_pq64"}else{"diskann_fp16"},
        "count":count,"list_size":effort,"rerank":rerank,"storage_precision":"fp16","queries":200,
        "pq_codes_bytes":pq.as_ref().map_or(0,PqIndex::code_bytes),"batch_ms":batch_ms,"qps":200000./batch_ms,
        "mean_ms":latencies.iter().sum::<f64>()/200.,"p50_ms":latencies[100],"p95_ms":latencies[190]})
    );
    println!("{}", json!({"stage":"results","ids":ids}));
}
