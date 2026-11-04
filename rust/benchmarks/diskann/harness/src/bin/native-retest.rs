//! Query-only retest with complete-workload warmup and per-pass resource counters.
use serde_json::{Value, json};
use std::{fs::File, io::Read, time::Instant};
use zvec_rust::{
    Collection, CollectionOptions, ConfigBuilder, DataType, DiskannQueryParams, HnswQueryParams,
    IndexType,
};

const DIM: usize = 768;

fn usage() -> libc::rusage {
    let mut value = std::mem::MaybeUninit::uninit();
    // getrusage initializes the complete C structure on success.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, value.as_mut_ptr()) },
        0
    );
    unsafe { value.assume_init() }
}

fn counters(before: &libc::rusage, after: &libc::rusage) -> Value {
    let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    json!({
        "user_s": seconds(after.ru_utime) - seconds(before.ru_utime),
        "system_s": seconds(after.ru_stime) - seconds(before.ru_stime),
        "minor_faults": after.ru_minflt - before.ru_minflt,
        "major_faults": after.ru_majflt - before.ru_majflt,
        "voluntary_switches": after.ru_nvcsw - before.ru_nvcsw,
        "involuntary_switches": after.ru_nivcsw - before.ru_nivcsw,
    })
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 6, "engine index queries.f32 count effort");
    let engine = args[1].as_str();
    assert!(matches!(engine, "hnsw" | "diskann"));
    let count: usize = args[4].parse().unwrap();
    let effort: i32 = args[5].parse().unwrap();
    assert!(effort > 0);
    let before_open = usage();
    let open_start = Instant::now();
    zvec_rust::initialize(Some(&ConfigBuilder::new().num_threads(1))).unwrap();
    let mut options = CollectionOptions::new().unwrap();
    options.set_read_only(true).unwrap();
    assert!(options.enable_mmap());
    let collection = Collection::open(&args[2], Some(&options)).unwrap();
    let field = zg_zvec_fp16::vector_field_info(&collection, "embedding")
        .unwrap()
        .unwrap();
    assert_eq!(field.data_type, DataType::VectorFp16);
    assert_eq!(
        field.index_type,
        if engine == "hnsw" {
            IndexType::Hnsw
        } else {
            IndexType::Diskann
        }
    );
    assert_eq!(collection.stats().unwrap().doc_count as usize, count);
    let open_s = open_start.elapsed().as_secs_f64();
    let after_open = usage();
    let mut bytes = Vec::new();
    File::open(&args[3])
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes.len(), 220 * DIM * 4);
    let queries: Vec<Vec<f32>> = bytes
        .chunks_exact(DIM * 4)
        .map(|v| {
            v.as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect()
        })
        .collect();
    let search = |vector: &[f32]| {
        let mut request = zg_zvec_fp16::embedding_query("embedding", vector, 10).unwrap();
        request.set_include_vector(false).unwrap();
        if engine == "hnsw" {
            request
                .set_hnsw_params(HnswQueryParams::new(effort, 0.0, false, false))
                .unwrap();
        } else {
            request
                .set_diskann_params(DiskannQueryParams::new(effort))
                .unwrap();
        }
        let ids: Vec<String> = collection
            .query(&request)
            .unwrap()
            .iter()
            .map(|doc| doc.get_pk().unwrap().to_owned())
            .collect();
        assert_eq!(ids.len(), 10);
        ids
    };
    let before = usage();
    let start = Instant::now();
    for vector in &queries[..20] {
        search(vector);
    }
    let warmup20_s = start.elapsed().as_secs_f64();
    let warmup20_counters = counters(&before, &usage());
    let mut ids = Vec::with_capacity(200);
    let mut phases = Vec::with_capacity(7);
    let mut latencies = Vec::with_capacity(1000);
    let mut measured_s = 0.0;
    for pass in 0..7 {
        let before = usage();
        let start = Instant::now();
        for (i, vector) in queries[20..].iter().enumerate() {
            let query_start = Instant::now();
            let result = search(vector);
            if pass >= 2 {
                latencies.push(query_start.elapsed().as_secs_f64() * 1000.0);
            }
            if pass == 0 {
                ids.push(result);
            } else {
                assert_eq!(result, ids[i]);
            }
        }
        let wall_s = start.elapsed().as_secs_f64();
        let after = usage();
        if pass >= 2 {
            measured_s += wall_s;
        }
        phases.push(
            json!({"phase": if pass < 2 { "warmup" } else { "measured" },
            "pass": pass, "queries": 200, "wall_s": wall_s, "qps": 200.0 / wall_s,
            "resources": counters(&before, &after)}),
        );
    }
    assert_eq!(latencies.len(), 1000);
    latencies.sort_by(f64::total_cmp);
    println!(
        "{}",
        json!({"stage":"queried", "engine":engine, "count":count,
        "effort":effort, "queries":1000, "passes":5, "full_warmup_passes":2,
        "batch_s":measured_s, "qps":1000.0/measured_s,
        "mean_ms":latencies.iter().sum::<f64>() / 1000.0,
        "p50_ms":latencies[500], "p95_ms":latencies[950],
        "open_s":open_s, "open_resources":counters(&before_open, &after_open),
        "warmup20_s":warmup20_s, "warmup20_resources":warmup20_counters, "phases":phases})
    );
    println!("{}", json!({"stage":"results", "ids":ids}));
}
