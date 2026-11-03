use serde_json::json;
use std::{
    fs::File,
    io::{BufReader, Read},
    path::Path,
    time::Instant,
};
use zg_diskann::{Distance, VectorIndex, VectorPrecision};
use zvec_rust::{Collection, CollectionOptions, HnswQueryParams};

// Same deterministic query sequence as the memory benchmark's FP32 truth set.
fn vector(id: usize) -> Vec<f32> {
    let mut state = (id as u64 + 1).wrapping_mul(0x9e3779b97f4a7c15);
    let mut result = Vec::with_capacity(768);
    let mut norm = 0.0_f64;
    for _ in 0..768 {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let word = state.wrapping_mul(0x2545f4914f6cdd1d);
        let value = ((word >> 40) as f32 / 8_388_608.0) - 1.0;
        norm += f64::from(value) * f64::from(value);
        result.push(value);
    }
    let norm = norm.sqrt() as f32;
    for value in &mut result {
        *value /= norm;
    }
    result
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert!(
        matches!(args.len(), 5..=7),
        "engine index_path count search_effort [queries.f32] [diskann precision: fp32|fp16]"
    );
    let precision = match args.get(6).map(String::as_str).unwrap_or("fp32") {
        "fp32" => VectorPrecision::Fp32,
        "fp16" => VectorPrecision::Fp16,
        _ => panic!("unsupported precision"),
    };
    let engine = args[1].as_str();
    assert!(matches!(engine, "diskann" | "hnsw"));
    let count: usize = args[3].parse().unwrap();
    let list_size: usize = args[4].parse().unwrap();
    let mut query_file = args.get(5).map(|path| {
        let file = File::open(path).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 220 * 768 * 4);
        BufReader::new(file)
    });
    zvec_rust::initialize(None).unwrap();
    let mut collection = None;
    let mut diskann = None;
    if engine == "diskann" {
        diskann = Some(
            VectorIndex::open_with_precision(
                Path::new(&args[2]),
                768,
                Distance::Cosine,
                true,
                precision,
            )
            .unwrap(),
        );
    } else {
        let mut options = CollectionOptions::new().unwrap();
        options.set_read_only(true).unwrap();
        assert!(options.enable_mmap());
        collection = Some(Collection::open(&args[2], Some(&options)).unwrap());
    }
    let mut ids = Vec::with_capacity(200);
    let mut latencies = Vec::with_capacity(200);
    let mut batch_started = None;
    for i in 0..220 {
        if i == 20 {
            batch_started = Some(Instant::now());
        }
        let mut query = if let Some(reader) = &mut query_file {
            let mut bytes = [0_u8; 768 * 4];
            reader.read_exact(&mut bytes).unwrap();
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect()
        } else {
            vector(1_000_000 + i)
        };
        let call_started = Instant::now();
        if engine == "diskann" && precision == VectorPrecision::Fp16 {
            // Match HNSW's binary16 query payload; retain the original FP32 GT.
            for value in &mut query {
                *value = half::f16::from_f32(*value).to_f32();
            }
        }
        let found: Vec<String> = if let Some(index) = &diskann {
            index
                .search_with_list_size(&query, 10, None, list_size)
                .unwrap()
                .into_iter()
                .map(|hit| hit.document_id)
                .collect()
        } else {
            let mut query = zg_zvec_fp16::embedding_query("embedding", &query, 10).unwrap();
            query.set_include_vector(false).unwrap();
            if list_size > 0 {
                query
                    .set_hnsw_params(HnswQueryParams::new(
                        i32::try_from(list_size).unwrap(),
                        0.0,
                        false,
                        false,
                    ))
                    .unwrap();
            }
            collection
                .as_ref()
                .unwrap()
                .query(&query)
                .unwrap()
                .iter()
                .map(|doc| doc.get_pk().unwrap().to_owned())
                .collect()
        };
        let elapsed_ms = call_started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(found.len(), 10);
        if i >= 20 {
            latencies.push(elapsed_ms);
            ids.push(found);
        }
    }
    let batch_ms = batch_started.unwrap().elapsed().as_secs_f64() * 1000.0;
    latencies.sort_by(f64::total_cmp);
    println!(
        "{}",
        json!({"stage":"queried","engine":engine,"count":count,"list_size":list_size,
            "storage_precision":if engine == "hnsw" || precision == VectorPrecision::Fp16 { "fp16" } else { "fp32" },
            "queries":200,"batch_ms":batch_ms,"qps":200_000.0/batch_ms,
            "mean_ms":latencies.iter().sum::<f64>()/200.0,
            "p50_ms":latencies[100],"p95_ms":latencies[190]})
    );
    println!("{}", json!({"stage":"results","ids":ids}));
}
