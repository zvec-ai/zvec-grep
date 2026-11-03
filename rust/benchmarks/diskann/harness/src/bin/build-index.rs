use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
    time::Instant,
};
use zg_diskann::{Distance, Record, VectorIndex, VectorPrecision};
use zvec_rust::{
    Collection, CollectionOptions, CollectionSchema, DataType, Doc, IndexParams, MetricType,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert!(
        matches!(args.len(), 6 | 7),
        "engine index_path base.f32 count start [fp32|fp16]"
    );
    let precision = match args.get(6).map(String::as_str).unwrap_or("fp32") {
        "fp32" => VectorPrecision::Fp32,
        "fp16" => VectorPrecision::Fp16,
        _ => panic!("unsupported precision"),
    };
    let engine = args[1].as_str();
    assert!(matches!(engine, "hnsw" | "diskann"));
    let path = Path::new(&args[2]);
    let count: usize = args[4].parse().unwrap();
    let start: usize = args[5].parse().unwrap();
    assert!(start < count);
    if start == 0 {
        assert!(!path.exists(), "Refusing to overwrite an index");
    } else {
        assert!(engine == "diskann" && path.exists());
    }
    let file = File::open(&args[3]).unwrap();
    assert!(file.metadata().unwrap().len() >= (count * 768 * 4) as u64);
    let mut input = BufReader::new(file);
    input
        .seek(SeekFrom::Start((start * 768 * 4) as u64))
        .unwrap();
    zvec_rust::initialize(None).unwrap();
    let mut diskann = None;
    let mut hnsw = None;
    if engine == "diskann" {
        diskann = Some(
            VectorIndex::open_with_precision(path, 768, Distance::Cosine, false, precision)
                .unwrap(),
        );
    } else {
        let schema = CollectionSchema::builder("cohere_bench")
            .add_vector_field(
                "embedding",
                DataType::VectorFp16,
                768,
                IndexParams::hnsw(MetricType::Cosine, 50, 500).unwrap(),
            )
            .build()
            .unwrap();
        hnsw = Some(
            Collection::create_and_open(
                path.to_str().unwrap(),
                &schema,
                Some(&CollectionOptions::new().unwrap()),
            )
            .unwrap(),
        );
    }
    let began = Instant::now();
    for first in (start..count).step_by(256) {
        let end = (first + 256).min(count);
        let mut rows = Vec::with_capacity(end - first);
        for id in first..end {
            let mut bytes = [0_u8; 768 * 4];
            input.read_exact(&mut bytes).unwrap();
            let vector: Vec<f32> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect();
            rows.push((format!("v{id:08}"), vector));
        }
        if let Some(index) = &diskann {
            let records: Vec<_> = rows
                .iter()
                .map(|(id, vector)| Record {
                    document_id: id,
                    file_id: 1,
                    vector,
                })
                .collect();
            index.upsert(&records).unwrap();
        } else {
            let docs: Vec<_> = rows
                .iter()
                .map(|(id, vector)| {
                    let mut doc = Doc::new().unwrap();
                    doc.set_pk(id);
                    zg_zvec_fp16::add_embedding(&mut doc, "embedding", vector).unwrap();
                    doc
                })
                .collect();
            let result = hnsw
                .as_ref()
                .unwrap()
                .upsert(&docs.iter().collect::<Vec<_>>())
                .unwrap();
            assert_eq!(result.error_count, 0);
            assert_eq!(result.success_count as usize, docs.len());
        }
        if (end - start) % 2048 == 0 || end == count {
            println!(
                "{}",
                serde_json::json!({"inserted":end,"elapsed_s":began.elapsed().as_secs_f64()})
            );
        }
    }
    if let Some(index) = &hnsw {
        index.flush().unwrap();
        index.optimize().unwrap();
        index.flush().unwrap();
        let stats = index.stats().unwrap();
        assert_eq!(stats.doc_count as usize, count);
        assert!(stats.indexes.iter().all(|i| i.completeness == 1.0));
    }
    println!(
        "{}",
        serde_json::json!({"stage":"built","engine":engine,"count":count,"elapsed_s":began.elapsed().as_secs_f64()})
    );
}
