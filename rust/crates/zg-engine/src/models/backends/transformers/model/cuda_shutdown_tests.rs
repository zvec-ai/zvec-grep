use super::*;

const CHILD: &str = "ZG_TEST_ORT_CUDA_SHUTDOWN_CHILD";
const TEST: &str = concat!(module_path!(), "::cached_minilm_cuda_sessions_exit_cleanly");

#[test]
#[ignore = "requires CUDA and ZVEC_GREP_TEST_MODEL_CACHE with pinned MiniLM; run by Rust CUDA CI"]
fn cached_minilm_cuda_sessions_exit_cleanly() {
    if let Ok(scenario) = env::var(CHILD) {
        tracing_subscriber::fmt()
            .with_env_filter("warn,zg_engine=debug,ort=trace")
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .try_init()
            .expect("retain CUDA graph placement evidence");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(exercise_shutdown(&scenario));
        println!("CUDA shutdown scenario completed: {scenario}");
        return;
    }
    // The test body can pass before an atexit callback corrupts the heap. Always
    // wait for a fresh process to exit; CI also runs these children in Memcheck.
    for scenario in ["load-failure", "reload"] {
        for perturb in [false, true] {
            let mut child =
                std::process::Command::new(env::current_exe().expect("test executable"));
            // libtest names omit the crate component in module_path!().
            child
                .args([
                    TEST.split_once("::").expect("crate prefix").1,
                    "--exact",
                    "--ignored",
                    "--nocapture",
                ])
                .env(CHILD, scenario)
                .env("ZVEC_GREP_ORT_THREAD_BUDGET", "2");
            if cfg!(target_os = "linux") {
                if perturb {
                    child
                        .env("MALLOC_PERTURB_", "165")
                        .env("GLIBC_TUNABLES", "glibc.malloc.tcache_count=0");
                } else {
                    child
                        .env_remove("MALLOC_PERTURB_")
                        .env_remove("GLIBC_TUNABLES");
                }
            }
            let output = child.output().expect("CUDA shutdown child");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!("scenario={scenario} perturb={perturb}\n{stdout}\n{stderr}");
            assert!(
                output.status.success(),
                "CUDA shutdown failed: {}",
                output.status
            );
            assert!(stdout.contains(&format!("CUDA shutdown scenario completed: {scenario}")));
        }
    }
}

async fn exercise_shutdown(scenario: &str) {
    let invalid = tempfile::NamedTempFile::new().expect("invalid model fixture");
    std::fs::write(invalid.path(), b"invalid ONNX model").expect("write invalid model");
    // Register/load the CUDA provider but fail to create any session. It must
    // still be safe to exit, and a subsequent valid model must remain usable.
    let error = load_session(invalid.path(), TransformersExecutionProvider::Cuda)
        .expect_err("invalid model must fail after provider registration");
    assert!(
        error
            .to_string()
            .contains("Unable to load cuda ONNX embedding model"),
        "{error}"
    );
    if scenario == "load-failure" {
        return;
    }
    assert_eq!(scenario, "reload");
    let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
        .map(PathBuf::from)
        .expect("pinned model cache");
    let entry = crate::models::catalog::get_embedding_model_catalog_entry("local/all-minilm-l6-v2")
        .and_then(crate::models::catalog::EmbeddingCatalogEntry::transformers_config)
        .expect("MiniLM catalog entry");
    let inputs = [vec![Content::Text(
        "find the telescope calibration code".to_owned(),
    )]];
    for generation in 0..2 {
        let model = TransformersEmbeddingModel::new(
            entry,
            ModelConfig {
                cache_dir: Some(cache.clone()),
                device: Some(Device::Cuda),
                ..ModelConfig::default()
            },
            crate::models::runtime::ModelComputeRuntime::shared(),
            2,
        );
        model
            .prepare(EmbeddingPrepareOptions::default())
            .await
            .expect("prepare two CUDA sessions");
        let loaded = Arc::clone(
            lock_std_mutex(&model.state)
                .as_ref()
                .expect("prepared model"),
        );
        assert_eq!(
            loaded.sessions.provider(),
            TransformersExecutionProvider::Cuda,
            "CPU fallback is forbidden"
        );
        assert_eq!(loaded.sessions.stats().session_count, 2);
        assert_eq!(loaded.sessions.stats().session_initializations, 2);
        let results = futures_util::future::join_all((0..4).map(|_| {
            model.embed(
                &inputs,
                EmbeddingOptions {
                    execution_concurrency: 2,
                    ..EmbeddingOptions::default()
                },
            )
        }))
        .await;
        for result in results {
            let vectors = result.expect("CUDA inference").vectors;
            assert_eq!(vectors.len(), 1);
            assert_eq!(vectors[0].len(), entry.dimension);
            assert!(vectors[0].iter().all(|value| value.is_finite()));
        }
        let signal = CancellationToken::new();
        signal.cancel();
        let cancelled = model
            .embed(
                &inputs,
                EmbeddingOptions {
                    signal: Some(signal),
                    ..EmbeddingOptions::default()
                },
            )
            .await;
        assert_eq!(
            cancelled.expect_err("cancelled request").code(),
            crate::EngineError::CANCELLED
        );
        drop(loaded);
        drop(model); // Joins both owning threads and releases both sessions.
        println!("CUDA generation={generation} sessions=2 released");
    }
}
