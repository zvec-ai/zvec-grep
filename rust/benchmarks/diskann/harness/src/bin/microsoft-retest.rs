//! Previous Microsoft Rust DiskANN + SQLite/PQ adapter, matching native-retest timing.
use serde_json::{Value, json};
use std::{fs::File, io::Read, path::Path, time::Instant};
use zg_diskann::{VectorIndex, experimental_pq::PqIndex};

const DIM: usize = 768;

fn usage() -> libc::rusage {
    let mut value = std::mem::MaybeUninit::uninit();
    // getrusage initializes the C structure on success.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, value.as_mut_ptr()) },
        0
    );
    unsafe { value.assume_init() }
}

fn counters(before: &libc::rusage, after: &libc::rusage) -> Value {
    let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    json!({"user_s":seconds(after.ru_utime)-seconds(before.ru_utime),
        "system_s":seconds(after.ru_stime)-seconds(before.ru_stime),
        "minor_faults":after.ru_minflt-before.ru_minflt,
        "major_faults":after.ru_majflt-before.ru_majflt,
        "voluntary_switches":after.ru_nvcsw-before.ru_nvcsw,
        "involuntary_switches":after.ru_nivcsw-before.ru_nivcsw})
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 6, "index queries.f32 count L codes.bin");
    let count: usize = args[3].parse().unwrap();
    let effort: usize = args[4].parse().unwrap();
    assert!(effort >= 128);
    let before_open = usage();
    let start_open = Instant::now();
    // Keep the same application-runtime initialization as the native harness.
    // Vector search below calls the Microsoft graph/PQ libraries through SQLite.
    zvec_rust::initialize(Some(&zvec_rust::ConfigBuilder::new().num_threads(1))).unwrap();
    let index = VectorIndex::open_read_only(Path::new(&args[1])).unwrap();
    let pq = PqIndex::load(&index, Path::new(&args[5]), count).unwrap();
    assert_eq!(pq.code_bytes(), count * 64);
    let open_s = start_open.elapsed().as_secs_f64();
    let after_open = usage();
    let mut bytes = Vec::new();
    File::open(&args[2])
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes.len(), 220 * DIM * 4);
    let queries: Vec<Vec<f32>> = bytes
        .as_chunks::<{ DIM * 4 }>()
        .0
        .iter()
        .map(|v| {
            v.as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect()
        })
        .collect();
    let search = |vector: &[f32]| {
        let query: Vec<_> = vector
            .iter()
            .map(|x| half::f16::from_f32(*x).to_f32())
            .collect();
        let result: Vec<_> = index
            .search_with_pq(&pq, &query, 10, effort, 128)
            .unwrap()
            .into_iter()
            .map(|hit| hit.document_id)
            .collect();
        assert_eq!(result.len(), 10);
        result
    };
    let before = usage();
    let start = Instant::now();
    for vector in &queries[..20] {
        search(vector);
    }
    let warmup20_s = start.elapsed().as_secs_f64();
    let warmup20_resources = counters(&before, &usage());
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
        phases.push(json!({"phase":if pass < 2 {"warmup"} else {"measured"},
            "pass":pass, "queries":200, "wall_s":wall_s, "qps":200.0/wall_s,
            "resources":counters(&before,&after)}));
    }
    assert_eq!(latencies.len(), 1000);
    latencies.sort_by(f64::total_cmp);
    println!(
        "{}",
        json!({"stage":"queried", "engine":"microsoft", "count":count,
        "effort":effort, "rerank":128, "pq_codes_bytes":pq.code_bytes(),
        "queries":1000, "passes":5, "full_warmup_passes":2, "batch_s":measured_s,
        "qps":1000.0/measured_s, "mean_ms":latencies.iter().sum::<f64>()/1000.0,
        "p50_ms":latencies[500], "p95_ms":latencies[950], "open_s":open_s,
        "open_resources":counters(&before_open,&after_open), "warmup20_s":warmup20_s,
        "warmup20_resources":warmup20_resources, "phases":phases})
    );
    println!("{}", json!({"stage":"results", "ids":ids}));
}
