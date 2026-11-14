//! Disposable local comparison of FP16/HNSW, IVF-RaBitQ, and packed sign scans.
//!
//! Run from `rust/` with:
//! `cargo run -p zg-zvec-fp16 --example vector_index_quantization -- <fp16|rabitq|binary-sign> [count] [dimension]`
//! The binary-sign mode stores packed bytes and scans/scorers float queries
//! against the sign reconstruction in a disposable local collection.

use std::{collections::HashSet, env, fs, io, path::Path, time::Instant};

#[cfg(not(target_os = "linux"))]
use std::io::ErrorKind;

use tempfile::tempdir;
use zg_zvec_fp16::{add_embedding, embedding_query};
use zvec_rust::{
    Collection, CollectionSchema, DataType, Doc, FieldSchema, IndexParams, IvfRabitqQueryParams,
    MetricType, SearchQuery,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    zvec_rust::initialize(None)?;
    let args = env::args().skip(1).collect::<Vec<_>>();
    let mode = args.first().map(String::as_str).unwrap_or("binary-sign");
    if mode != "fp16" && mode != "rabitq" && mode != "binary-sign" {
        return Err("mode must be fp16, rabitq, or binary-sign".into());
    }
    let count = parse_arg(&args, 1, 20_000_usize)?;
    let dimension = parse_arg(&args, 2, 1_024_usize)?;
    if count < 2 || dimension == 0 || !dimension.is_multiple_of(32) {
        return Err("count must be >= 2 and dimension a positive multiple of 32".into());
    }

    let directory = tempdir()?;
    let collection_path = directory.path().join("vectors");
    if mode == "binary-sign" && count > 100_000 {
        return Err("binary-sign probe is limited to 100,000 rows".into());
    }
    let mut schema = CollectionSchema::builder("vectors");
    match mode {
        "fp16" => {
            schema = schema.add_vector_field(
                "embedding",
                DataType::VectorFp16,
                dimension as u32,
                IndexParams::hnsw(MetricType::Cosine, 50, 500)?,
            );
        }
        "rabitq" => {
            schema = schema.add_vector_field(
                "embedding",
                DataType::VectorFp32,
                dimension as u32,
                IndexParams::ivf_rabitq(MetricType::Cosine, 1, 7, 0)?,
            );
        }
        "binary-sign" => {
            schema = schema
                .add_field(FieldSchema::new("embedding", DataType::Binary, false, 0)?)
                .add_field(FieldSchema::new(
                    "embedding_scale",
                    DataType::Float,
                    false,
                    0,
                )?);
            let mut bucket = FieldSchema::new("vector_scan_bucket", DataType::Uint32, false, 0)?;
            bucket.set_index_params(&IndexParams::invert(true, false)?)?;
            schema = schema.add_field(bucket);
        }
        _ => unreachable!(),
    }
    let schema = schema.build()?;
    let collection = Collection::create_and_open(
        collection_path
            .to_str()
            .ok_or("non-Unicode temporary path")?,
        &schema,
        None,
    )?;

    let exact_query = query_vector(dimension);
    let mut exact_scores = (0..count)
        .map(|id| {
            (
                id,
                cosine_similarity(&exact_query, &document_vector(id, dimension)),
            )
        })
        .collect::<Vec<_>>();
    exact_scores.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    let exact_top = exact_scores
        .iter()
        .take(10)
        .map(|(id, _)| *id)
        .collect::<HashSet<_>>();
    drop(exact_scores);

    let build_started = Instant::now();
    const BATCH: usize = 128;
    for start in (0..count).step_by(BATCH) {
        let end = (start + BATCH).min(count);
        let mut docs = Vec::with_capacity(end - start);
        for id in start..end {
            let vector = document_vector(id, dimension);
            let mut doc = Doc::new()?;
            doc.set_pk(&format!("doc-{id:08}"));
            match mode {
                "fp16" => add_embedding(&mut doc, "embedding", &vector)?,
                "rabitq" => doc.add_vector_f32("embedding", &vector)?,
                "binary-sign" => {
                    let (packed, scale) = encode_sign_vector(&vector);
                    doc.add_binary("embedding", &packed)?;
                    doc.add_f32("embedding_scale", scale)?;
                    doc.add_u32("vector_scan_bucket", vector_scan_bucket(id)?)?;
                }
                _ => unreachable!(),
            }
            docs.push(doc);
        }
        let references = docs.iter().collect::<Vec<_>>();
        let result = collection.insert(&references)?;
        if result.success_count != u64::try_from(end - start)? || result.error_count != 0 {
            return Err(format!("insert failed: {result:?}").into());
        }
    }
    collection.optimize()?;
    collection.flush()?;
    let build_millis = build_started.elapsed().as_millis();

    let query_started = Instant::now();
    let hit_ids = if mode == "binary-sign" {
        binary_sign_search(&collection, &exact_query, count)?
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>()
    } else {
        let mut request = if mode == "fp16" {
            embedding_query("embedding", &exact_query, 10)?
        } else {
            let mut request = SearchQuery::new("embedding", &exact_query, 10)?;
            request.set_ivf_rabitq_params(IvfRabitqQueryParams::new(1, 0.0, false, false))?;
            request
        };
        request.set_include_vector(false)?;
        collection
            .query(&request)?
            .into_iter()
            .map(|document| {
                let key = document.get_pk().ok_or("document has no primary key")?;
                Ok(key
                    .strip_prefix("doc-")
                    .ok_or("unexpected primary key")?
                    .parse::<usize>()?)
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?
    };
    let query_millis = query_started.elapsed().as_millis();
    let overlap = hit_ids.iter().filter(|id| exact_top.contains(id)).count();
    let disk_bytes = directory_size(&collection_path)?;
    let (rss_bytes, pss_bytes) = linux_memory_bytes()?;
    println!(
        "mode={mode} vectors={count} dimensions={dimension} vector_payload_bytes_per_record={} index_bytes={disk_bytes} rss_bytes={rss_bytes:?} pss_bytes={pss_bytes:?} build_ms={build_millis} query_ms={query_millis} exact_top10_overlap={overlap}/10",
        match mode {
            "fp16" => dimension * 2,
            "rabitq" => dimension * 4,
            "binary-sign" => dimension.div_ceil(8) + std::mem::size_of::<f32>(),
            _ => unreachable!(),
        },
    );
    collection.close()?;
    Ok(())
}

fn parse_arg<T: std::str::FromStr>(
    args: &[String],
    index: usize,
    default: T,
) -> Result<T, Box<dyn std::error::Error>>
where
    T::Err: std::error::Error + 'static,
{
    args.get(index)
        .map_or(Ok(default), |value| Ok(value.parse()?))
}

fn document_vector(id: usize, dimension: usize) -> Vec<f32> {
    let mut state = (id as u64)
        .wrapping_add(1)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (0..dimension)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let magnitude = 0.001 + ((state >> 32) as u32) as f32 / u32::MAX as f32 * 0.1;
            if state & 1 == 0 {
                -magnitude
            } else {
                magnitude
            }
        })
        .collect()
}

fn query_vector(dimension: usize) -> Vec<f32> {
    let mut state = 0x4d65_7469_7351_7565_u64;
    (0..dimension)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let magnitude = 0.001 + ((state >> 32) as u32) as f32 / u32::MAX as f32 * 0.1;
            if state & 0x1_0000 != 0 {
                magnitude
            } else {
                -magnitude
            }
        })
        .collect()
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> f64 {
    let dot = left
        .iter()
        .zip(right)
        .map(|(left, right)| f64::from(*left) * f64::from(*right))
        .sum::<f64>();
    let left_norm = left
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>();
    let right_norm = right
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>();
    dot / (left_norm * right_norm).sqrt()
}

fn encode_sign_vector(vector: &[f32]) -> (Vec<u8>, f32) {
    let mut packed = vec![0_u8; vector.len().div_ceil(u8::BITS as usize)];
    let mut absolute_sum = 0.0_f64;
    for (dimension, value) in vector.iter().enumerate() {
        absolute_sum += f64::from(value.abs());
        if *value >= 0.0 {
            packed[dimension / u8::BITS as usize] |= 1_u8 << (dimension % u8::BITS as usize);
        }
    }
    (packed, (absolute_sum / vector.len() as f64) as f32)
}

fn vector_scan_bucket(id: usize) -> Result<u32, Box<dyn std::error::Error>> {
    let id = u32::try_from(id)?;
    Ok(id.wrapping_mul(0x9e37_79b9) >> 16)
}

fn binary_sign_search(
    collection: &Collection,
    query: &[f32],
    count: usize,
) -> Result<Vec<(usize, f64)>, Box<dyn std::error::Error>> {
    let mut scores = Vec::with_capacity(count);
    for document in collection.iter_with_options(Some(&["embedding", "embedding_scale"]), false)? {
        let document = document?;
        let key = document.get_pk().ok_or("document has no primary key")?;
        let id = key
            .strip_prefix("doc-")
            .ok_or("unexpected primary key")?
            .parse::<usize>()?;
        let packed = document
            .get_binary("embedding")?
            .ok_or("document has no packed embedding")?;
        let scale = document
            .get_f32("embedding_scale")?
            .ok_or("document has no embedding scale")?;
        if !scale.is_finite() || scale < 0.0 {
            return Err("invalid embedding scale".into());
        }
        let query_dot = query
            .iter()
            .enumerate()
            .map(|(dimension, value)| {
                let positive = packed[dimension / u8::BITS as usize]
                    & (1_u8 << (dimension % u8::BITS as usize))
                    != 0;
                f64::from(*value) * if positive { 1.0 } else { -1.0 }
            })
            .sum::<f64>();
        let query_norm = query
            .iter()
            .map(|value| f64::from(*value).powi(2))
            .sum::<f64>()
            .sqrt();
        let score = if scale == 0.0 || query_norm == 0.0 {
            0.0
        } else {
            query_dot / (query_norm * (query.len() as f64).sqrt())
        };
        scores.push((id, score));
    }
    scores.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    scores.truncate(10);
    Ok(scores)
}

fn directory_size(path: &Path) -> io::Result<u64> {
    let mut size = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            size = size.saturating_add(metadata.len());
        } else if metadata.is_dir() {
            size = size.saturating_add(directory_size(&entry.path())?);
        }
    }
    Ok(size)
}

fn linux_memory_bytes() -> io::Result<(Option<u64>, Option<u64>)> {
    #[cfg(target_os = "linux")]
    {
        let contents = fs::read_to_string("/proc/self/smaps_rollup")?;
        let mut rss = None;
        let mut pss = None;
        for line in contents.lines() {
            let mut fields = line.split_whitespace();
            match fields.next() {
                Some("Rss:") => rss = fields.next().and_then(|value| value.parse::<u64>().ok()),
                Some("Pss:") => pss = fields.next().and_then(|value| value.parse::<u64>().ok()),
                _ => {}
            }
        }
        return Ok((rss.map(|kib| kib * 1024), pss.map(|kib| kib * 1024)));
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "resident-memory sampling requires Linux /proc/self/smaps_rollup",
        ))
    }
}
