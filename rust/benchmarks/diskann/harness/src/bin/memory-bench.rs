//! One pass over unseen queries after a separate, disjoint warmup set.
use serde_json::{Value, json};
use std::{collections::HashSet, fs, path::Path, time::Instant};
use zg_diskann::{VectorIndex, experimental_pq::PqIndex};
use zvec_rust::{
    Collection, CollectionOptions, ConfigBuilder, DataType, DiskannQueryParams, IndexType,
};

const DIM: usize = 768;

fn usage() -> libc::rusage {
    let mut value = std::mem::MaybeUninit::uninit();
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

fn queries(path: &str) -> Vec<Vec<f32>> {
    let bytes = fs::read(path).unwrap();
    assert!(!bytes.is_empty() && bytes.len().is_multiple_of(DIM * 4));
    bytes
        .chunks_exact(DIM * 4)
        .map(|row| {
            let v: Vec<_> = row
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect();
            assert!(v.iter().all(|x| x.is_finite()));
            v
        })
        .collect()
}

enum Engine {
    Native(Collection),
    Microsoft(VectorIndex, PqIndex),
}

impl Engine {
    fn search(&self, vector: &[f32], effort: usize) -> Vec<String> {
        let ids = match self {
            Self::Native(collection) => {
                let mut request = zg_zvec_fp16::embedding_query("embedding", vector, 10).unwrap();
                request.set_include_vector(false).unwrap();
                request
                    .set_diskann_params(DiskannQueryParams::new(effort as i32))
                    .unwrap();
                collection
                    .query(&request)
                    .unwrap()
                    .iter()
                    .map(|d| d.get_pk().unwrap().to_owned())
                    .collect::<Vec<_>>()
            }
            Self::Microsoft(index, pq) => {
                let q: Vec<_> = vector
                    .iter()
                    .map(|x| half::f16::from_f32(*x).to_f32())
                    .collect();
                index
                    .search_with_pq(pq, &q, 10, effort, 128)
                    .unwrap()
                    .into_iter()
                    .map(|hit| hit.document_id)
                    .collect()
            }
        };
        assert_eq!(ids.len(), 10);
        ids
    }
}

fn verify_library() {
    let expected = std::env::var("ZVEC_BENCH_EXPECT_LIBRARY").expect("expected library path");
    let loaded = unsafe {
        let symbol = libc::dlsym(libc::RTLD_DEFAULT, c"zvec_initialize".as_ptr());
        assert!(!symbol.is_null());
        let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
        assert_ne!(libc::dladdr(symbol, info.as_mut_ptr()), 0);
        let info = info.assume_init();
        assert!(!info.dli_fname.is_null());
        std::ffi::CStr::from_ptr(info.dli_fname)
            .to_str()
            .unwrap()
            .to_owned()
    };
    let loaded = fs::canonicalize(loaded).unwrap();
    assert_eq!(
        loaded,
        fs::canonicalize(expected).unwrap(),
        "wrong native library loaded"
    );
    println!("{}", json!({"stage":"library", "path":loaded}));
}

fn main() {
    verify_library();
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        8,
        "backend index warmup.f32 test.f32 count L codes.bin"
    );
    let backend = args[1].as_str();
    assert!(matches!(backend, "zvec" | "microsoft"));
    let count: usize = args[5].parse().unwrap();
    let effort: usize = args[6].parse().unwrap();
    assert!(effort > 0 && effort <= i32::MAX as usize);
    let warmup = queries(&args[3]);
    let measured = queries(&args[4]);
    // Reject duplicated queries within or across the two sets before any search.
    let mut unique = HashSet::new();
    for row in warmup.iter().chain(measured.iter()) {
        assert!(
            unique.insert(row.iter().map(|x| x.to_bits()).collect::<Vec<_>>()),
            "warmup/test overlap or duplicate query"
        );
    }
    drop(unique);
    let before_open = usage();
    let start = Instant::now();
    zvec_rust::initialize(Some(&ConfigBuilder::new().num_threads(1))).unwrap();
    let engine = if backend == "zvec" {
        let mut options = CollectionOptions::new().unwrap();
        options.set_read_only(true).unwrap();
        assert!(options.enable_mmap());
        let collection = Collection::open(&args[2], Some(&options)).unwrap();
        let field = zg_zvec_fp16::vector_field_info(&collection, "embedding")
            .unwrap()
            .unwrap();
        assert_eq!(field.data_type, DataType::VectorFp16);
        assert_eq!(field.index_type, IndexType::Diskann);
        assert_eq!(collection.stats().unwrap().doc_count as usize, count);
        Engine::Native(collection)
    } else {
        assert!(effort >= 128);
        let index = VectorIndex::open_read_only(Path::new(&args[2])).unwrap();
        let pq = PqIndex::load(&index, Path::new(&args[7]), count).unwrap();
        assert_eq!(pq.code_bytes(), count * 64);
        Engine::Microsoft(index, pq)
    };
    let open_s = start.elapsed().as_secs_f64();
    let after_open = usage();
    let start = Instant::now();
    for query in &warmup {
        engine.search(query, effort);
    }
    let warmup_s = start.elapsed().as_secs_f64();
    let after_warmup = usage();
    let mut ids = Vec::with_capacity(measured.len());
    let mut latencies = Vec::with_capacity(measured.len());
    let start = Instant::now();
    for query in &measured {
        let t = Instant::now();
        let result = engine.search(query, effort);
        latencies.push(t.elapsed().as_secs_f64() * 1000.0);
        ids.push(result);
    }
    let batch_s = start.elapsed().as_secs_f64();
    let after = usage();
    let ordered_latencies = latencies.clone();
    latencies.sort_by(f64::total_cmp);
    let q = measured.len();
    println!(
        "{}",
        json!({"stage":"queried","backend":backend,"count":count,"effort":effort,
        "warmup_queries":warmup.len(),"queries":q,"test_passes":1,"test_warmup_passes":0,
        "batch_s":batch_s,"qps":q as f64/batch_s,"p50_ms":latencies[q/2],"p95_ms":latencies[q*95/100],
        "latencies_ms":ordered_latencies,"open_s":open_s,"warmup_s":warmup_s,
        "rss_peak_before_open_mib":before_open.ru_maxrss as f64/1048576.0,
        "rss_peak_after_open_mib":after_open.ru_maxrss as f64/1048576.0,
        "rss_peak_after_warmup_mib":after_warmup.ru_maxrss as f64/1048576.0,
        "rss_peak_after_test_mib":after.ru_maxrss as f64/1048576.0,
        "open_resources":counters(&before_open,&after_open),
        "warmup_resources":counters(&after_open,&after_warmup),"measured_resources":counters(&after_warmup,&after)})
    );
    println!("{}", json!({"stage":"results","ids":ids}));
}
