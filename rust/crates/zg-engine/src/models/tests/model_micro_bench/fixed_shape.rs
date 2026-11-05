//! Fixed nominal input size and batch shape, using production CLI concurrency.
use std::sync::Mutex;

use futures_util::future::join_all;
use sha2::{Digest, Sha256};

use super::{
    Arc, Config, Content, Data, Duration, EmbeddingOptions, EmbeddingPrepareOptions,
    EmbeddingPurpose, Instant, ModelRuntimeLease, ModelRuntimeManager, Row, Value, emit_rows, json,
    nanos, number, phase, seconds, summary, support,
};

const GENERATOR: &str = "nominal-words-v1";
const WORDS: [&str; 8] = [
    "search", "store", "read", "update", "check", "process", "share", "return",
];

fn inputs(target: usize, batch: usize) -> Data {
    let mut data = Data {
        inputs: Vec::new(),
        batch,
        nominal_tokens: target,
        descriptions: Vec::new(),
    };
    for index in 0..WORDS.len() {
        // L is a configured input-size label. No tokenizer probe or model
        // correctness check is performed by this performance benchmark.
        let text = (0..target)
            .map(|offset| WORDS[(index + offset) % WORDS.len()])
            .collect::<Vec<_>>()
            .join(" ");
        data.descriptions.push(json!({
            "id":format!("input-{index:02}"),"generator":GENERATOR,
            "text_sha256":hex::encode(Sha256::digest(text.as_bytes())),"bytes":text.len(),
        }));
        data.inputs.push(vec![Content::Text(text)]);
    }
    data
}

fn indices(id: usize, data: &Data) -> Vec<usize> {
    (0..data.batch)
        .map(|i| (id * data.batch + i) % data.inputs.len())
        .collect()
}

fn batch(data: &Data, id: usize) -> Vec<Vec<Content>> {
    indices(id, data)
        .into_iter()
        .map(|i| data.inputs[i].clone())
        .collect()
}

fn cycle_batches(inputs: usize, batch: usize) -> usize {
    let (mut a, mut b) = (inputs, batch);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    inputs / a
}

fn embedding_options(fixed_batch: bool) -> EmbeddingOptions {
    EmbeddingOptions {
        purpose: EmbeddingPurpose::Document,
        fixed_batch,
        ..Default::default()
    }
}

async fn warmup(
    model: &ModelRuntimeLease,
    data: &Data,
    workers: usize,
    fixed_batch: bool,
) -> Value {
    let shapes = cycle_batches(data.inputs.len(), data.batch);
    let waves = shapes.max(3);
    for wave in 0..waves {
        let input = batch(data, wave % shapes);
        let results = join_all(
            (0..workers).map(|_| model.embed(&input, embedding_options(fixed_batch), None)),
        )
        .await;
        for result in results {
            result.expect("embedding warmup");
        }
    }
    json!({"worker_waves":waves,"workers":workers,"batch_shapes":shapes})
}

#[derive(Default)]
struct Schedule {
    next: usize,
    completed: usize,
    minimum: usize,
    cycle: usize,
    stop_at: Option<usize>,
    failed: bool,
    capped: bool,
}

impl Schedule {
    fn take(&mut self, elapsed: Duration, duration: Duration, maximum: Duration) -> Option<usize> {
        if self.failed || self.capped {
            return None;
        }
        if elapsed >= maximum {
            self.capped = true;
            return None;
        }
        if self.stop_at.is_none() && elapsed >= duration && self.completed >= self.minimum {
            self.stop_at = Some(self.next.div_ceil(self.cycle) * self.cycle);
        }
        if self.stop_at.is_some_and(|end| self.next >= end) {
            return None;
        }
        let id = self.next;
        self.next += 1;
        Some(id)
    }

    fn finish(&mut self, successful: bool) {
        if successful {
            self.completed += 1;
        } else {
            self.failed = true;
        }
    }
}

async fn request(
    model: &ModelRuntimeLease,
    data: &Data,
    origin: Instant,
    id: usize,
    fixed_batch: bool,
) -> Row {
    let selected = indices(id, data);
    let texts: Vec<_> = selected.iter().map(|&i| data.inputs[i].clone()).collect();
    let options = embedding_options(fixed_batch);
    let started = Instant::now();
    let output = model.embed(&texts, options, None).await;
    let finished = Instant::now();
    let status = if output.is_ok() { "completed" } else { "error" };
    if let Err(error) = &output {
        eprintln!("embedding batch {id} failed: {error}");
    }
    let vectors = output.map_or(0, |result| result.vectors.len());
    Row {
        id,
        scheduled_ns: nanos(started.duration_since(origin)),
        issued_ns: nanos(started.duration_since(origin)),
        finished_ns: nanos(finished.duration_since(origin)),
        status,
        vectors,
        input_indices: selected,
    }
}

async fn measure(
    model: &Arc<ModelRuntimeLease>,
    data: &Arc<Data>,
    workers: usize,
    origin: Instant,
    fixed_batch: bool,
) -> (Vec<Row>, Value) {
    let duration = seconds(number("ZG_MODEL_BENCH_SECONDS", 15));
    let maximum = seconds(number("ZG_MODEL_BENCH_MAX_SECONDS", 120));
    let minimum = number("ZG_MODEL_BENCH_MIN_BATCHES", 100);
    assert!(
        maximum >= duration,
        "maximum duration must cover target duration"
    );
    let schedule = Arc::new(Mutex::new(Schedule {
        minimum,
        cycle: cycle_batches(data.inputs.len(), data.batch),
        ..Schedule::default()
    }));
    let started = Instant::now();
    let tasks = (0..workers).map(|_| {
        let model = Arc::clone(model);
        let data = Arc::clone(data);
        let schedule = Arc::clone(&schedule);
        tokio::spawn(async move {
            let mut rows = Vec::new();
            loop {
                let id = schedule.lock().expect("shape schedule").take(
                    started.elapsed(),
                    duration,
                    maximum,
                );
                let Some(id) = id else { break };
                let row = request(&model, &data, origin, id, fixed_batch).await;
                schedule
                    .lock()
                    .expect("shape schedule")
                    .finish(row.status == "completed");
                rows.push(row);
            }
            rows
        })
    });
    let mut rows = Vec::new();
    for result in join_all(tasks).await {
        rows.extend(result.expect("fixed-shape worker"));
    }
    rows.sort_by_key(|row| row.id);
    let start_ns = nanos(started.duration_since(origin));
    let finish_ns = rows
        .iter()
        .map(|row| row.finished_ns)
        .max()
        .unwrap_or(start_ns);
    let mut measured = summary(
        &rows,
        Duration::from_nanos(finish_ns.saturating_sub(start_ns).max(1)),
        data.batch * data.nominal_tokens,
    );
    let schedule = schedule.lock().expect("final shape schedule");
    measured["started_ns"] = json!(start_ns);
    measured["finished_ns"] = json!(finish_ns);
    measured["target_duration_seconds"] = json!(duration.as_secs_f64());
    measured["maximum_duration_seconds"] = json!(maximum.as_secs_f64());
    measured["minimum_batches"] = json!(minimum);
    measured["minimum_sample_met"] = json!(schedule.completed >= minimum);
    measured["stopped_by_cap"] = json!(schedule.capped);
    measured["protocol_pass"] =
        json!(!rows.is_empty() && rows.iter().all(|r| r.status == "completed"));
    (rows, measured)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual CPU/Metal/CUDA performance benchmark; cached artifacts and release build required"]
async fn local_model_shape() {
    #[expect(clippy::assertions_on_constants)]
    {
        assert!(!cfg!(debug_assertions), "use --release");
    }
    let config = Config::environment();
    let target = number("ZG_MODEL_BENCH_TOKENS", 32);
    let requested_batch = number("ZG_MODEL_BENCH_BATCH_SIZE", 1);
    let fixed_batch = std::env::var("ZG_MODEL_BENCH_FIXED_BATCH").is_ok_and(|v| v == "1");
    let mut provenance = support::verify_artifacts(config.entry, &config.cache).await;
    let manager = ModelRuntimeManager::new();
    let origin = Instant::now();
    phase("before_load", origin);
    tokio::time::sleep(Duration::from_millis(150)).await;
    phase("load", origin);
    let load_started = Instant::now();
    let model = config.acquire(&manager);
    let workers = config.workers(&model);
    let acquire_ns = nanos(load_started.elapsed());
    let prepare_started = Instant::now();
    model
        .prepare(EmbeddingPrepareOptions::default(), None)
        .await
        .expect("prepare model");
    let prepare_ns = nanos(prepare_started.elapsed());
    let load_ns = nanos(load_started.elapsed());
    phase("loaded", origin);
    tokio::time::sleep(Duration::from_millis(150)).await;
    phase("inputs", origin);
    let data = Arc::new(inputs(target, requested_batch));
    provenance["inputs"] = json!(data.descriptions);
    provenance["corpus_version"] = json!(GENERATOR);
    provenance["corpus_sha256"] = json!(hex::encode(Sha256::digest(
        serde_json::to_vec(&data.descriptions).expect("input metadata")
    )));
    provenance["input_template"] = json!("Document; deterministic whitespace-separated words");
    provenance["input_length_semantics"] = json!("nominal");
    phase("warmup", origin);
    let warmup = warmup(&model, &data, workers, fixed_batch).await;
    let execution_before = model.execution_stats();
    phase("embedding", origin);
    let (rows, mut embedding) = measure(&model, &data, workers, origin, fixed_batch).await;
    phase("between", origin);
    embedding["execution"] =
        support::execution_measurement(execution_before, model.execution_stats());
    embedding["warmup"] = warmup;
    emit_rows("embedding", &rows);
    println!(
        "ZG_BENCH_RESULT={}",
        json!({"schema_version":5,"protocol_pass":embedding["protocol_pass"],
            "provenance":provenance,"input_tokens":target,"batch_size":requested_batch,
            "fixed_batch":fixed_batch,
            "embedding_concurrency":config.requested,"resolved_index_concurrency":config.index_limit,
            "effective_execution_concurrency":model.execution_concurrency(),
            "admission_concurrency":model.admission_concurrency(),
            "index_inflight_batches":workers,"requested_device":format!("{:?}",config.device).to_lowercase(),
            "load":{"cache_state":"fresh_process_filesystem_cache_warm_or_unspecified",
                "acquire_ns":acquire_ns,"prepare_ns":prepare_ns,"total_ns":load_ns},
            "embedding":embedding})
    );
    drop(model);
    manager.close();
}

#[test]
fn schedule_uses_completed_samples_and_finishes_a_balanced_cycle() {
    let mut schedule = Schedule {
        minimum: 3,
        cycle: 4,
        ..Schedule::default()
    };
    for expected in 0..4 {
        assert_eq!(
            schedule.take(seconds(1), seconds(1), seconds(10)),
            Some(expected)
        );
    }
    assert_eq!(schedule.completed, 0);
    for _ in 0..3 {
        schedule.finish(true);
    }
    assert_eq!(schedule.take(seconds(1), seconds(1), seconds(10)), None);
    assert!(!schedule.capped);
}

#[test]
fn schedule_caps_submission_and_stops_after_failure_without_losing_inflight_ids() {
    let mut capped = Schedule {
        minimum: 100,
        cycle: 8,
        ..Schedule::default()
    };
    assert_eq!(capped.take(seconds(1), seconds(1), seconds(10)), Some(0));
    assert_eq!(capped.take(seconds(10), seconds(1), seconds(10)), None);
    assert!(capped.capped);
    capped.finish(true);
    assert_eq!(capped.completed, 1);
    let mut failed = Schedule {
        minimum: 100,
        cycle: 8,
        ..Schedule::default()
    };
    assert_eq!(failed.take(seconds(1), seconds(1), seconds(10)), Some(0));
    assert_eq!(failed.take(seconds(1), seconds(1), seconds(10)), Some(1));
    failed.finish(false);
    assert_eq!(failed.take(seconds(1), seconds(1), seconds(10)), None);
    failed.finish(true);
    assert_eq!(failed.next, 2);
    assert_eq!(failed.completed, 1);
}

#[test]
fn corpus_cycle_handles_batches_smaller_and_larger_than_the_pool() {
    assert_eq!(cycle_batches(8, 1), 8);
    assert_eq!(cycle_batches(8, 4), 2);
    assert_eq!(cycle_batches(8, 16), 1);
}
