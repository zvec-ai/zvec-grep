use crate::{
    domain::model::{Device, ModelConfig},
    models::{
        ModelRuntimeLease, ModelRuntimeManager, ModelRuntimeRequest,
        artifacts::ArtifactDownloadConfig,
        catalog::{
            EmbeddingCatalogEntry, get_embedding_model_catalog_entry, list_embedding_models,
        },
        runtime::{batch_concurrency, resolve_index_concurrency},
        spi::EmbeddingPrepareOptions,
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
        // Same successful-work admission as indexing's resolve_index_batch_policy.
        batch_concurrency(&self.reference, self.index_limit)
            .unwrap_or(model.concurrency_defaults().initial)
    }
}

fn download(entry: EmbeddingCatalogEntry) -> &'static ArtifactDownloadConfig {
    match entry {
        EmbeddingCatalogEntry::LlamaCpp(e) => e.download,
        EmbeddingCatalogEntry::Model2Vec(e) => e.download,
        EmbeddingCatalogEntry::Transformers(e) => e.download,
        EmbeddingCatalogEntry::Qwen(_) => panic!("local models only"),
    }
}

fn roots(entry: EmbeddingCatalogEntry, cache: &Path) -> [PathBuf; 2] {
    let d = download(entry);
    match entry {
        EmbeddingCatalogEntry::LlamaCpp(_) => [cache.to_owned(), cache.to_owned()],
        EmbeddingCatalogEntry::Model2Vec(_) => [
            cache
                .join("model2vec")
                .join(d.hugging_face.repo.replace('/', "--"))
                .join(d.hugging_face.revision),
            cache
                .join("modelscope/model2vec")
                .join(d.model_scope.repo.replace('/', "--"))
                .join(d.model_scope.revision),
        ],
        EmbeddingCatalogEntry::Transformers(_) => [
            cache
                .join(d.hugging_face.repo)
                .join(d.hugging_face.revision),
            cache
                .join("modelscope/transformers-js")
                .join(d.model_scope.repo.replace('/', "--"))
                .join(d.model_scope.revision),
        ],
        EmbeddingCatalogEntry::Qwen(_) => panic!("local only"),
    }
}

fn local_path(entry: EmbeddingCatalogEntry, artifact: &str) -> String {
    match entry {
        EmbeddingCatalogEntry::LlamaCpp(e) => e.cache_file.to_owned(),
        EmbeddingCatalogEntry::Model2Vec(e) if artifact == e.tokenizer_file => {
            "tokenizer/tokenizer.json".into()
        }
        EmbeddingCatalogEntry::Model2Vec(e) if artifact == e.model_file => Path::new(artifact)
            .file_name()
            .expect("model filename")
            .to_str()
            .expect("UTF8")
            .into(),
        _ => artifact.into(),
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

pub(super) fn verify_artifacts(config: &Config) -> Value {
    let mut meta = metadata(config.entry);
    let candidates = roots(config.entry, &config.cache);
    let root = candidates
        .iter()
        .find(|r| {
            download(config.entry)
                .artifacts
                .iter()
                .all(|a| r.join(local_path(config.entry, a.path)).is_file())
        })
        .expect("download artifacts before measuring");
    let mut verified = Vec::new();
    for artifact in download(config.entry).artifacts {
        let path = root.join(local_path(config.entry, artifact.path));
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
    println!("ZG_BENCH_SETUP={}", verify_artifacts(&config));
    drop(model);
    manager.close();
}

#[test]
fn cli_concurrency_controls_batch_workers() {
    for (reference, requested, expected) in [
        ("local/embeddinggemma-300m", Some(4), 1),
        ("local/all-minilm-l6-v2", Some(24), 8),
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
