//! Both backends use the same zvec SDK, document layout, precision and query path.
use serde_json::json;
use std::{
    fs::{self, File},
    io::{BufReader, Read},
    path::Path,
    time::Instant,
};
use zvec_rust::{
    Collection, CollectionOptions, CollectionSchema, ConfigBuilder, DataType, DiskannQueryParams,
    Doc, HnswQueryParams, IndexParams, IndexType, MetricType,
};

const DIM: usize = 768;
fn read_vector(input: &mut impl Read) -> Vec<f32> {
    let mut bytes = [0_u8; DIM * 4];
    input.read_exact(&mut bytes).unwrap();
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
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
        "build|query engine index input.f32 count effort passes"
    );
    let build = args[1] == "build";
    assert!(build || args[1] == "query");
    let engine = args[2].as_str();
    assert!(matches!(engine, "hnsw" | "diskann"));
    let count: usize = args[5].parse().unwrap();
    let effort: usize = args[6].parse().unwrap();
    let passes: usize = args[7].parse().unwrap();
    assert!(passes > 0);
    // One request at a time, one query worker; builds get eight workers on both sides.
    zvec_rust::initialize(Some(&ConfigBuilder::new().num_threads(if build {
        8
    } else {
        1
    })))
    .unwrap();
    if build {
        assert!(
            !Path::new(&args[3]).exists(),
            "Refusing to overwrite an index"
        );
        let params = if engine == "diskann" {
            IndexParams::diskann(
                MetricType::Cosine,
                std::env::var("ZVEC_BENCH_BUILD_DEGREE")
                    .unwrap()
                    .parse()
                    .unwrap(),
                std::env::var("ZVEC_BENCH_BUILD_LIST")
                    .unwrap()
                    .parse()
                    .unwrap(),
                64,
            )
            .unwrap()
        } else {
            IndexParams::hnsw(MetricType::Cosine, 50, 500).unwrap()
        };
        let schema = CollectionSchema::builder("cohere_native_bench")
            .add_vector_field("embedding", DataType::VectorFp16, DIM as u32, params)
            .build()
            .unwrap();
        let collection = Collection::create_and_open(&args[3], &schema, None).unwrap();
        let file = File::open(&args[4]).unwrap();
        assert!(file.metadata().unwrap().len() >= (count * DIM * 4) as u64);
        let mut input = BufReader::new(file);
        let started = Instant::now();
        for first in (0..count).step_by(256) {
            let end = (first + 256).min(count);
            let docs: Vec<_> = (first..end)
                .map(|id| {
                    let mut doc = Doc::new().unwrap();
                    doc.set_pk(&format!("v{id:08}"));
                    zg_zvec_fp16::add_embedding(&mut doc, "embedding", &read_vector(&mut input))
                        .unwrap();
                    doc
                })
                .collect();
            let result = collection.upsert(&docs.iter().collect::<Vec<_>>()).unwrap();
            assert_eq!(result.error_count, 0);
            assert_eq!(result.success_count as usize, docs.len());
            if end % 10240 == 0 || end == count {
                println!(
                    "{}",
                    json!({"stage":"insert","count":end,"elapsed_s":started.elapsed().as_secs_f64()})
                );
            }
        }
        collection.flush().unwrap();
        println!(
            "{}",
            json!({"stage":"optimize","elapsed_s":started.elapsed().as_secs_f64()})
        );
        collection.optimize().unwrap();
        collection.flush().unwrap();
        let stats = collection.stats().unwrap();
        assert_eq!(stats.doc_count as usize, count);
        assert!(!stats.indexes.is_empty());
        assert!(
            stats
                .indexes
                .iter()
                .all(|i| (i.completeness - 1.0).abs() < f32::EPSILON)
        );
        println!(
            "{}",
            json!({"stage":"built","engine":engine,"count":count,"elapsed_s":started.elapsed().as_secs_f64()})
        );
        return;
    }
    let mut options = CollectionOptions::new().unwrap();
    options.set_read_only(true).unwrap();
    assert!(options.enable_mmap());
    let collection = Collection::open(&args[3], Some(&options)).unwrap();
    let field = zg_zvec_fp16::vector_field_info(&collection, "embedding")
        .unwrap()
        .unwrap();
    assert_eq!(field.data_type, DataType::VectorFp16);
    assert_eq!(
        field.index_type,
        if engine == "diskann" {
            IndexType::Diskann
        } else {
            IndexType::Hnsw
        }
    );
    assert_eq!(collection.stats().unwrap().doc_count as usize, count);
    let file = File::open(&args[4]).unwrap();
    assert_eq!(file.metadata().unwrap().len(), (220 * DIM * 4) as u64);
    let mut input = BufReader::new(file);
    let queries: Vec<_> = (0..220).map(|_| read_vector(&mut input)).collect();
    let search = |vector: &[f32]| {
        let mut request = zg_zvec_fp16::embedding_query("embedding", vector, 10).unwrap();
        request.set_include_vector(false).unwrap();
        if engine == "diskann" {
            request
                .set_diskann_params(DiskannQueryParams::new(effort.try_into().unwrap()))
                .unwrap();
        } else {
            request
                .set_hnsw_params(HnswQueryParams::new(
                    effort.try_into().unwrap(),
                    0.0,
                    false,
                    false,
                ))
                .unwrap();
        }
        let ids: Vec<_> = collection
            .query(&request)
            .unwrap()
            .iter()
            .map(|doc| doc.get_pk().unwrap().to_owned())
            .collect();
        assert_eq!(ids.len(), 10);
        ids
    };
    for vector in &queries[..20] {
        search(vector);
    }
    let mut results = Vec::with_capacity(200);
    let mut latencies = Vec::with_capacity(200 * passes);
    let started = Instant::now();
    for pass in 0..passes {
        for (i, vector) in queries[20..].iter().enumerate() {
            let call = Instant::now();
            let ids = search(vector);
            latencies.push(call.elapsed().as_secs_f64() * 1000.0);
            if pass == 0 {
                results.push(ids);
            } else {
                assert_eq!(ids, results[i]);
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    latencies.sort_by(f64::total_cmp);
    let n = latencies.len();
    println!(
        "{}",
        json!({"stage":"queried","engine":engine,"count":count,"effort":effort,
        "queries":n,"passes":passes,"batch_s":elapsed,"qps":n as f64 / elapsed,
        "mean_ms":latencies.iter().sum::<f64>()/n as f64,
        "p50_ms":latencies[n/2],"p95_ms":latencies[n*95/100]})
    );
    println!("{}", json!({"stage":"results","ids":results}));
}
