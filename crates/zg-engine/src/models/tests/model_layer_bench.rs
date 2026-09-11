use std::{
    env,
    hint::black_box,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

use futures_util::future::join_all;
use serde_json::json;

use crate::{
    api::index::options::Device,
    models::spi::{EmbeddingInput, EmbeddingOptions},
};

use super::super::{
    ModelRuntimeLease, ModelRuntimeManager, ModelRuntimeRequest, spi::CreateEmbeddingModelOptions,
};

const DEFAULT_MODEL_REFERENCE: &str = "local/potion-code-16m-v2";

/// Manual, release-mode comparison harness shared with
/// `benchmarks/model-layer/main.mjs`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual model-layer throughput and RSS benchmark"]
async fn model_layer_throughput() {
    if env_flag("ZG_MODEL_BENCH_BASELINE") {
        std::thread::sleep(std::time::Duration::from_millis(250));
        println!(
            "ZG_MODEL_BENCH_JSON={}",
            json!({ "implementation": "rust", "mode": "baseline" })
        );
        return;
    }

    let batch_size = env_usize("ZG_MODEL_BENCH_BATCH", 256);
    let concurrency = env_usize("ZG_MODEL_BENCH_CONCURRENCY", 1);
    let vectors_per_round = env_usize("ZG_MODEL_BENCH_VECTORS", 16_384);
    let rounds = env_usize("ZG_MODEL_BENCH_ROUNDS", 5);
    let warmup_waves = env_usize("ZG_MODEL_BENCH_WARMUP_WAVES", 2);
    let model_reference =
        env::var("ZG_MODEL_BENCH_MODEL").unwrap_or_else(|_| DEFAULT_MODEL_REFERENCE.to_owned());
    let device = env::var("ZG_MODEL_BENCH_DEVICE")
        .ok()
        .map(|value| parse_device(&value));
    assert!(batch_size > 0, "batch size must be positive");
    assert!(concurrency > 0, "concurrency must be positive");
    assert!(rounds > 0, "round count must be positive");
    let vectors_per_wave = batch_size
        .checked_mul(concurrency)
        .expect("vectors per wave should fit usize");
    assert_eq!(
        vectors_per_round % vectors_per_wave,
        0,
        "vectors per round must be divisible by batch * concurrency"
    );

    let cache = env::var_os("ZG_MODEL_BENCH_CACHE").map(PathBuf::from);
    let manager = ModelRuntimeManager::new();
    let model = create_benchmark_model(&manager, &model_reference, cache, device, concurrency);
    assert!(
        batch_size <= model.info().limits.max_batch_size,
        "batch size exceeds model limit"
    );
    let batch = benchmark_batch(batch_size);

    for _ in 0..warmup_waves {
        black_box(run_wave(&model, &batch, concurrency).await);
    }
    std::thread::sleep(Duration::from_millis(100));
    let loaded_rss_bytes = current_rss_bytes();

    let waves_per_round = vectors_per_round / vectors_per_wave;
    let mut elapsed_seconds = Vec::with_capacity(rounds);
    let mut checksum = 0.0_f64;
    for _ in 0..rounds {
        let started = Instant::now();
        for _ in 0..waves_per_round {
            checksum += run_wave(&model, &batch, concurrency).await;
        }
        elapsed_seconds.push(started.elapsed().as_secs_f64());
    }
    black_box(checksum);
    drop(model);
    manager.close();

    let vectors_per_round_f64 = count_as_f64(vectors_per_round, "vectors per round");
    let vectors_per_second = elapsed_seconds
        .iter()
        .map(|seconds| vectors_per_round_f64 / seconds)
        .collect::<Vec<_>>();
    let requests_per_round = waves_per_round * concurrency;
    let requests_per_round_f64 = count_as_f64(requests_per_round, "requests per round");
    let requests_per_second = elapsed_seconds
        .iter()
        .map(|seconds| requests_per_round_f64 / seconds)
        .collect::<Vec<_>>();
    println!(
        "ZG_MODEL_BENCH_JSON={}",
        json!({
            "implementation": "rust",
            "mode": "model",
            "model": model_reference,
            "device": device.map(device_name),
            "batch_size": batch_size,
            "concurrency": concurrency,
            "vectors_per_round": vectors_per_round,
            "rounds": rounds,
            "warmup_waves": warmup_waves,
            "loaded_rss_bytes": loaded_rss_bytes,
            "elapsed_seconds": elapsed_seconds,
            "vectors_per_second": vectors_per_second,
            "requests_per_second": requests_per_second,
            "checksum": checksum,
        })
    );
}

fn create_benchmark_model(
    manager: &ModelRuntimeManager,
    model_reference: &str,
    cache: Option<PathBuf>,
    device: Option<Device>,
    concurrency: usize,
) -> ModelRuntimeLease {
    manager
        .acquire(ModelRuntimeRequest::new(
            model_reference,
            CreateEmbeddingModelOptions {
                api_key: env::var("DASHSCOPE_API_KEY").ok(),
                model_cache_dir: cache,
                device,
                ..CreateEmbeddingModelOptions::default()
            },
            Some(concurrency),
        ))
        .expect("benchmark model runtime should be acquired")
}

fn parse_device(value: &str) -> Device {
    match value {
        "auto" => Device::Auto,
        "cpu" => Device::Cpu,
        "metal" => Device::Metal,
        "vulkan" => Device::Vulkan,
        "cuda" => Device::Cuda,
        _ => panic!("invalid ZG_MODEL_BENCH_DEVICE={value:?}"),
    }
}

const fn device_name(device: Device) -> &'static str {
    match device {
        Device::Auto => "auto",
        Device::Cpu => "cpu",
        Device::Metal => "metal",
        Device::Vulkan => "vulkan",
        Device::Cuda => "cuda",
    }
}

async fn run_wave(model: &ModelRuntimeLease, batch: &[EmbeddingInput], concurrency: usize) -> f64 {
    let results =
        join_all((0..concurrency).map(|_| model.embed(batch, EmbeddingOptions::default(), None)))
            .await;
    results
        .into_iter()
        .map(|result| {
            result
                .expect("benchmark embedding should succeed")
                .vectors
                .iter()
                .map(|vector| {
                    f64::from(vector.first().copied().unwrap_or_default())
                        + f64::from(vector.last().copied().unwrap_or_default())
                })
                .sum::<f64>()
        })
        .sum()
}

fn benchmark_batch(batch_size: usize) -> Vec<EmbeddingInput> {
    (0..batch_size)
        .map(|index| {
            EmbeddingInput::text(format!(
                "pub fn benchmark_{index}(value: usize) -> usize {{ let adjusted = value.wrapping_mul(31).wrapping_add({index}); adjusted ^ 0x5a5a }}"
            ))
        })
        .collect()
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name).map_or(default, |value| {
        value
            .parse::<usize>()
            .unwrap_or_else(|error| panic!("invalid {name}={value:?}: {error}"))
    })
}

fn env_flag(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| value != "0" && value != "false")
}

fn count_as_f64(value: usize, label: &str) -> f64 {
    f64::from(u32::try_from(value).unwrap_or_else(|error| panic!("{label} exceeds u32: {error}")))
}

fn current_rss_bytes() -> u64 {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps should report benchmark RSS");
    assert!(output.status.success(), "ps should exit successfully");
    let kibibytes = String::from_utf8(output.stdout)
        .expect("ps RSS should be UTF-8")
        .trim()
        .parse::<u64>()
        .expect("ps RSS should be an integer");
    kibibytes
        .checked_mul(1_024)
        .expect("benchmark RSS should fit u64")
}
