use crate::{
    domain::model::{Device, ModelConfig},
    models::{
        ModelRuntimeLease, ModelRuntimeManager, ModelRuntimeRequest,
        artifacts::{ArtifactDownloadConfig, cached_source},
        catalog::{
            EmbeddingCatalogEntry, get_embedding_model_catalog_entry, list_embedding_models,
        },
        runtime::resolve_index_concurrency,
        spi::{EmbeddingPrepareOptions, ModelExecutionStats},
    },
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(super) struct Config {
    pub reference: String,
    pub cache: PathBuf,
    pub device: Device,
    pub requested: Option<usize>,
    pub index_limit: Option<usize>,
    pub entry: EmbeddingCatalogEntry,
}

impl Config {
    pub fn environment() -> Self {
        let reference = env::var("ZG_MODEL_BENCH_MODEL").expect("explicit local model");
        let entry = get_embedding_model_catalog_entry(&reference).expect("catalog model");
        assert!(
            !matches!(entry, EmbeddingCatalogEntry::Qwen(_)),
            "remote models are not authorized"
        );
        let requested = match env::var("ZG_MODEL_BENCH_CONCURRENCY").as_deref() {
            Ok("default") | Err(_) => None,
            Ok(value) => Some(value.parse().expect("CLI positive concurrency")),
        };
        let index_limit =
            resolve_index_concurrency(&reference, requested).expect("CLI concurrency resolution");
        let device = match env::var("ZG_MODEL_BENCH_DEVICE").as_deref() {
            Ok("cpu") => Device::Cpu,
            Ok("metal") => Device::Metal,
            #[cfg(feature = "cuda")]
            Ok("cuda") => Device::Cuda,
            #[cfg(not(feature = "cuda"))]
            Ok("cuda") => panic!("CUDA requires a CUDA build"),
            _ => panic!("device must be cpu, metal or cuda"),
        };
        Self {
            reference,
            entry,
            requested,
            index_limit,
            device,
            cache: PathBuf::from(env::var_os("ZG_MODEL_BENCH_CACHE").expect("explicit cache")),
        }
    }

    pub fn acquire(&self, manager: &ModelRuntimeManager) -> Arc<ModelRuntimeLease> {
        Arc::new(
            manager
                .acquire(ModelRuntimeRequest::new(
                    &self.reference,
                    ModelConfig {
                        cache_dir: Some(self.cache.clone()),
                        device: Some(self.device),
                        ..Default::default()
                    },
                    self.index_limit,
                ))
                .expect("runtime acquisition"),
        )
    }

    pub fn workers(&self, model: &ModelRuntimeLease) -> usize {
        if let Ok(value) = env::var("ZG_MODEL_BENCH_CLIENTS") {
            let clients = value
                .parse::<usize>()
                .expect("positive benchmark client count");
            assert!(clients > 0, "positive benchmark client count");
            return clients;
        }
        // Same successful-work admission as indexing's resolve_index_batch_policy.
        model
            .index_batch_concurrency(self.index_limit)
            .unwrap_or(model.concurrency_defaults().initial)
    }

    pub fn client_mode() -> &'static str {
        if env::var_os("ZG_MODEL_BENCH_CLIENTS").is_some() {
            "fixed"
        } else {
            "production"
        }
    }
}

pub(super) fn execution_measurement(
    before: Option<ModelExecutionStats>,
    after: Option<ModelExecutionStats>,
) -> Value {
    let (Some(before), Some(after)) = (before, after) else {
        return Value::Null;
    };
    let mut measured = after.clone();
    macro_rules! difference {
        ($($field:ident),+ $(,)?) => {$(
            measured.$field = after.$field.checked_sub(before.$field)
                .expect("execution counters must be monotonic");
        )+};
    }
    difference!(
        prepared_batches,
        pool_reconfigurations,
        native_runs,
        native_rows,
        padded_tokens,
        preparation_ns,
        inference_ns,
        postprocessing_ns,
        backend_to_native_ns,
    );
    json!({
        "before": before, "after": after, "measured": measured,
        "counter_scope": "measured_embedding_phase",
        "peak_scope": "runtime_including_warmup",
        "timing_scope": {
            "backend_to_native_ns": "per request: backend submission to native start; includes preparation and scheduling; excludes runtime/backend admission; overlaps preparation_ns",
            "work_ns": "preparation/inference/postprocessing are summed work durations, not disjoint end-to-end latency components",
        },
    })
}

fn download(entry: EmbeddingCatalogEntry) -> &'static ArtifactDownloadConfig {
    match entry {
        EmbeddingCatalogEntry::LlamaCpp(e) => e.download,
        EmbeddingCatalogEntry::Model2Vec(e) => e.download,
        EmbeddingCatalogEntry::Transformers(e) => e.download,
        EmbeddingCatalogEntry::Qwen(_) => panic!("local models only"),
    }
}

fn metadata(entry: EmbeddingCatalogEntry) -> Value {
    let (batch, limit, semantics) = match entry {
        EmbeddingCatalogEntry::LlamaCpp(e) => (
            e.max_batch_size,
            e.context_size,
            json!({"format":e.format,"dtype":"Q8_0","pooling":"native model metadata","normalize":false}),
        ),
        EmbeddingCatalogEntry::Model2Vec(e) => (
            e.max_batch_size,
            e.max_input_tokens,
            json!({"dtype":"safetensors","normalize":e.normalize,"pooling":"mean excluding unknown ids","query_prefix":e.query_prefix,"document_prefix":e.document_prefix}),
        ),
        EmbeddingCatalogEntry::Transformers(e) => (
            e.max_batch_size,
            e.max_input_tokens,
            json!({"dtype":e.dtype,"pooling":e.pooling,"normalize":e.normalize,"query_prefix":e.query_prefix,"document_prefix":e.document_prefix}),
        ),
        EmbeddingCatalogEntry::Qwen(_) => panic!("local only"),
    };
    let d = download(entry);
    json!({"model":entry.reference(),"backend":entry.backend(),"dimension":entry.dimension(),"max_batch_size":batch,
        "max_input_tokens":limit,"semantics":semantics,"revision":d.hugging_face.revision,
        "fallback_revision":d.model_scope.revision,"artifacts":d.artifacts.iter().map(|a|json!({"path":a.path,"size":a.size,"sha256":a.sha256})).collect::<Vec<_>>()})
}

pub(super) async fn verify_artifacts(entry: EmbeddingCatalogEntry, cache: &Path) -> Value {
    let mut meta = metadata(entry);
    let source = cached_source(entry, Some(cache))
        .await
        .expect("download artifacts before measuring");
    let mut verified = Vec::new();
    for artifact in download(entry).artifacts {
        let path = source.local_path(artifact).expect("catalog artifact path");
        let mut file = File::open(&path).expect("local artifact");
        assert_eq!(
            file.metadata().expect("artifact metadata").len(),
            artifact.size
        );
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let n = file.read(&mut buffer).expect("hash artifact");
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        let hash = hex::encode(hasher.finalize());
        assert_eq!(hash, artifact.sha256, "artifact integrity");
        verified.push(json!({"path":path,"sha256":hash,"size":artifact.size}));
    }
    meta["verified_artifacts"] = json!(verified);
    meta
}

#[test]
#[ignore = "export immutable catalog for the benchmark runner"]
fn inventory() {
    println!(
        "ZG_BENCH_INVENTORY={}",
        json!(
            list_embedding_models()
                .into_iter()
                .filter(|e| !matches!(e, EmbeddingCatalogEntry::Qwen(_)))
                .map(metadata)
                .collect::<Vec<_>>()
        )
    );
}

#[tokio::test]
#[ignore = "explicit setup: downloads pinned public weights; outside all measurements"]
async fn prepare_artifacts() {
    let config = Config::environment();
    let manager = ModelRuntimeManager::new();
    let model = config.acquire(&manager);
    model
        .prepare(EmbeddingPrepareOptions::default(), None)
        .await
        .expect("prepare artifacts");
    println!(
        "ZG_BENCH_SETUP={}",
        verify_artifacts(config.entry, &config.cache).await
    );
    drop(model);
    manager.close();
}

#[tokio::test]
async fn verifies_gguf_in_modelscope_cache() {
    use crate::models::artifacts::{ArtifactConfig, ArtifactSourceConfig};

    const SOURCE: ArtifactSourceConfig = ArtifactSourceConfig {
        repo: "fixture/gguf",
        revision: "v1",
    };
    const DOWNLOAD: ArtifactDownloadConfig = ArtifactDownloadConfig {
        hugging_face: SOURCE,
        model_scope: SOURCE,
        artifacts: &[ArtifactConfig {
            path: "model.gguf",
            size: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        }],
    };
    let EmbeddingCatalogEntry::LlamaCpp(mut model) =
        get_embedding_model_catalog_entry("local/embeddinggemma-300m").expect("catalog model")
    else {
        panic!("expected GGUF model")
    };
    model.download = &DOWNLOAD;
    let cache = tempfile::tempdir().expect("isolated cache");
    let snapshot = cache.path().join("modelscope/llama-cpp/fixture--gguf/v1");
    std::fs::create_dir_all(&snapshot).expect("create snapshot");
    let fallback = snapshot.join("model.gguf");
    std::fs::write(&fallback, b"abc").expect("write GGUF fixture");

    let record = verify_artifacts(EmbeddingCatalogEntry::LlamaCpp(model), cache.path()).await;
    let recorded_path = record["verified_artifacts"][0]["path"]
        .as_str()
        .expect("verified artifact path");
    assert_eq!(Path::new(recorded_path), fallback.as_path());
    assert_eq!(
        record["verified_artifacts"][0]["sha256"],
        DOWNLOAD.artifacts[0].sha256
    );
}

#[test]
fn cli_concurrency_controls_batch_workers() {
    for (reference, requested, expected) in [
        ("local/embeddinggemma-300m", Some(4), 1),
        ("local/potion-code-16m-v2", Some(24), 24),
        ("local/potion-code-16m-v2", None, 2),
    ] {
        let config = Config {
            reference: reference.into(),
            cache: PathBuf::from("/unused"),
            device: Device::Cpu,
            requested,
            index_limit: requested,
            entry: get_embedding_model_catalog_entry(reference).expect("model"),
        };
        let manager = ModelRuntimeManager::new();
        let model = config.acquire(&manager);
        assert_eq!(config.workers(&model), expected);
        drop(model);
        manager.close();
    }
}

#[test]
fn transformer_workers_use_effective_runtime_admission() {
    for requested in [None, Some(0), Some(1), Some(2), Some(usize::MAX)] {
        let reference = "local/all-minilm-l6-v2";
        let cache = tempfile::tempdir().expect("isolated cache");
        let config = Config {
            reference: reference.into(),
            cache: cache.path().into(),
            device: Device::Cpu,
            requested,
            index_limit: resolve_index_concurrency(reference, requested).expect("budget"),
            entry: get_embedding_model_catalog_entry(reference).expect("model"),
        };
        let manager = ModelRuntimeManager::new();
        let model = config.acquire(&manager);
        assert_eq!(config.workers(&model), model.admission_concurrency());
        assert!(model.execution_concurrency() >= 1);
        assert!(
            model.execution_concurrency()
                <= std::thread::available_parallelism()
                    .expect("available hardware threads")
                    .get()
        );
        drop(model);
        manager.close();
    }
}

#[test]
fn execution_measurement_excludes_warmup_and_labels_runtime_peaks() {
    let before = ModelExecutionStats {
        actual_provider: "cpu",
        session_count: 1,
        cpu_threads: 2,
        cpu_threads_total: 2,
        prepared_batches: 3,
        pool_reconfigurations: 1,
        native_runs: 3,
        native_rows: 12,
        padded_tokens: 384,
        preparation_ns: 30,
        inference_ns: 300,
        postprocessing_ns: 15,
        backend_to_native_ns: 9,
        max_native_batch_rows: 4,
        max_padded_tokens: 128,
        native_inflight_peak: 1,
    };
    let after = ModelExecutionStats {
        prepared_batches: 5,
        pool_reconfigurations: 2,
        native_runs: 5,
        native_rows: 20,
        padded_tokens: 640,
        preparation_ns: 50,
        inference_ns: 500,
        postprocessing_ns: 25,
        backend_to_native_ns: 15,
        ..before.clone()
    };
    let measured = execution_measurement(Some(before), Some(after));
    assert_eq!(measured["measured"]["native_runs"], 2);
    assert_eq!(measured["measured"]["native_rows"], 8);
    assert_eq!(measured["measured"]["padded_tokens"], 256);
    assert_eq!(measured["measured"]["preparation_ns"], 20);
    assert_eq!(measured["measured"]["inference_ns"], 200);
    assert_eq!(measured["measured"]["postprocessing_ns"], 10);
    assert_eq!(measured["measured"]["backend_to_native_ns"], 6);
    assert_eq!(measured["measured"]["pool_reconfigurations"], 1);
    assert!(measured["measured"].get("queue_wait_ns").is_none());
    assert!(
        measured["timing_scope"]["backend_to_native_ns"]
            .as_str()
            .expect("timing scope")
            .contains("overlaps preparation_ns")
    );
    assert_eq!(measured["measured"]["actual_provider"], "cpu");
    assert_eq!(measured["peak_scope"], "runtime_including_warmup");
    assert!(execution_measurement(None, None).is_null());
}
