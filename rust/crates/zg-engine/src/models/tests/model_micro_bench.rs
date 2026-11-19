//! Model inference micro-benchmark; see rust/micro-bench/model/README.md.
use std::{
    env,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::{
    domain::{Content, model::EmbeddingPurpose},
    models::{
        ModelRuntimeLease, ModelRuntimeManager,
        spi::{EmbeddingOptions, EmbeddingPrepareOptions},
    },
};

mod fixed_shape;
mod mixed_minilm;
mod support;
use support::Config;

struct Data {
    inputs: Vec<Vec<Content>>,
    batch: usize,
    nominal_tokens: usize,
    descriptions: Vec<Value>,
}

fn number(name: &str, default: usize) -> usize {
    let value = env::var(name).map_or(default, |s| {
        s.parse().expect("positive integer configuration")
    });
    assert!(value > 0, "positive benchmark configuration");
    value
}

fn seconds(value: usize) -> Duration {
    Duration::from_secs(u64::try_from(value).expect("duration fits u64"))
}

fn nanos(value: Duration) -> u64 {
    u64::try_from(value.as_nanos()).expect("duration fits u64 nanoseconds")
}

fn count(value: usize) -> f64 {
    f64::from(u32::try_from(value).expect("count fits u32"))
}

fn phase(name: &str, origin: Instant) {
    println!(
        "ZG_BENCH_EVENT={}",
        json!({"phase":name,"at_ns":nanos(origin.elapsed())})
    );
}

#[derive(Clone, serde::Serialize)]
struct Row {
    id: usize,
    scheduled_ns: u64,
    issued_ns: u64,
    finished_ns: u64,
    status: &'static str,
    vectors: usize,
    input_indices: Vec<usize>,
}

fn quantiles(mut values: Vec<u64>) -> Value {
    values.sort_unstable();
    let quantile = |percent: usize| {
        let rank = (values.len() * percent).div_ceil(100);
        values
            .get(rank.saturating_sub(1))
            .map(|&n| Duration::from_nanos(n).as_secs_f64() * 1000.0)
    };
    json!({"n":values.len(),"p50_ms":quantile(50),"p95_ms":if values.len()>=100 {quantile(95)} else {None},
        "p99_ms":if values.len()>=1000 {quantile(99)} else {None}})
}

fn summary(rows: &[Row], elapsed: Duration, nominal_tokens_per_batch: usize) -> Value {
    let successful: Vec<_> = rows.iter().filter(|r| r.status == "completed").collect();
    let completed_vectors: usize = successful.iter().map(|r| r.vectors).sum();
    json!({"scheduled":rows.len(),"issued":rows.len(),"completed":successful.len(),
        "timeout":rows.iter().filter(|r|r.status=="timeout").count(),"errors":rows.iter().filter(|r|r.status=="error").count(),
        "elapsed_seconds":elapsed.as_secs_f64(),"vectors_per_second":count(completed_vectors)/elapsed.as_secs_f64(),
        "nominal_tokens_per_second":count(successful.len())*count(nominal_tokens_per_batch)/elapsed.as_secs_f64(),
        "call_latency":quantiles(successful.iter().map(|r|r.finished_ns-r.issued_ns).collect())})
}

fn emit_rows(phase: &str, rows: &[Row]) {
    println!("ZG_BENCH_ROWS={}", json!({"phase":phase,"rows":rows}));
}

#[test]
fn nearest_rank_and_insufficient_tail_samples() {
    assert_eq!(quantiles((1..=1000).collect())["p99_ms"], json!(0.000_99));
    assert!(quantiles(vec![1; 99])["p95_ms"].is_null());
    assert!(quantiles(vec![1; 999])["p99_ms"].is_null());
    assert!(quantiles(Vec::new())["p50_ms"].is_null());
}
