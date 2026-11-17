use super::*;

const CHILD: &str = "ZG_TEST_ORT_CUDA_SHUTDOWN_CHILD";

#[test]
#[ignore = "requires CUDA and ZVEC_GREP_TEST_MODEL_CACHE with pinned MiniLM; run by Rust CUDA CI"]
fn cached_minilm_cuda_sessions_exit_cleanly() {
    let scenarios: &[&str] = if cfg!(target_os = "linux") {
        &["load-failure", "reload", "split-reload"]
    } else {
        &["load-failure", "reload"]
    };
    run_shutdown_scenarios(
        concat!(module_path!(), "::cached_minilm_cuda_sessions_exit_cleanly"),
        scenarios,
    );
}

#[test]
#[ignore = "requires CUDA dependencies, ZVEC_GREP_TEST_CPU_ORT and pinned MiniLM cache; run by Rust CUDA CI"]
fn cpu_core_rejects_cuda_without_loading_provider() {
    run_shutdown_scenarios(
        concat!(
            module_path!(),
            "::cpu_core_rejects_cuda_without_loading_provider"
        ),
        &["early-failure"],
    );
}

fn run_shutdown_scenarios(test: &str, scenarios: &[&str]) {
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
    for scenario in scenarios {
        for perturb in [false, true] {
            let mut child =
                std::process::Command::new(env::current_exe().expect("test executable"));
            // libtest names omit the crate component in module_path!().
            child
                .args([
                    test.split_once("::").expect("crate prefix").1,
                    "--exact",
                    "--ignored",
                    "--nocapture",
                ])
                .env(CHILD, scenario)
                .env("ZVEC_GREP_ORT_THREAD_BUDGET", "2");
            let _fixture = configure_scenario(&mut child, scenario);
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

fn configure_scenario(
    child: &mut std::process::Command,
    scenario: &str,
) -> Option<tempfile::TempDir> {
    if scenario == "early-failure" {
        let core = PathBuf::from(env::var_os("ZVEC_GREP_TEST_CPU_ORT").expect("CPU ORT fixture"));
        let directory = core.parent().expect("CPU core directory");
        let provider = if cfg!(target_os = "windows") {
            "onnxruntime_providers_cuda.dll"
        } else {
            "libonnxruntime_providers_cuda.so"
        };
        assert!(
            directory.join(provider).is_file(),
            "CUDA provider beside CPU core"
        );
        child.env("ORT_DYLIB_PATH", &core);
        #[cfg(target_os = "linux")]
        // Expose Provider_GetHost with an uninitialized host, making an illicit
        // first load reach CUDA's constructor instead of failing symbol lookup.
        child.env(
            "LD_PRELOAD",
            directory.join("libonnxruntime_providers_shared.so"),
        );
    } else if scenario == "split-reload" {
        let core = PathBuf::from(env::var_os("ORT_DYLIB_PATH").expect("CUDA ORT core"));
        let fixture = tempfile::tempdir().expect("separate core directory");
        let relocated = fixture.path().join("libonnxruntime.so.1");
        std::fs::copy(core, &relocated).expect("relocate only the core library");
        assert!(
            !fixture
                .path()
                .join("libonnxruntime_providers_cuda.so")
                .exists()
        );
        // Providers remain on LD_LIBRARY_PATH, outside the core's directory.
        child.env("ORT_DYLIB_PATH", relocated);
        return Some(fixture);
    }
    None
}

async fn exercise_shutdown(scenario: &str) {
    let early_failure = scenario == "early-failure";
    if early_failure {
        assert!(!zg_ort_native::retain_loaded_cuda_provider());
    }
    let invalid = tempfile::NamedTempFile::new().expect("invalid model fixture");
    std::fs::write(invalid.path(), b"invalid ONNX model").expect("write invalid model");
    // Fail either before provider loading (CPU core) or after registration
    // (invalid graph). Exiting and loading a subsequent valid model must be safe.
    let error = load_session(invalid.path(), TransformersExecutionProvider::Cuda)
        .expect_err("provider registration or model loading must fail");
    let expected_error = if early_failure {
        "Unable to configure CUDA ONNX embedding model"
    } else {
        "Unable to load cuda ONNX embedding model"
    };
    assert!(error.to_string().contains(expected_error), "{error}");
    if scenario == "load-failure" {
        return;
    }
    assert!(matches!(
        scenario,
        "reload" | "split-reload" | "early-failure"
    ));
    let expected_provider = if early_failure {
        TransformersExecutionProvider::Cpu
    } else {
        TransformersExecutionProvider::Cuda
    };
    exercise_sessions(expected_provider).await;
    if early_failure {
        assert!(
            !zg_ort_native::retain_loaded_cuda_provider(),
            "a CPU core must never cause CUDA to be loaded after registration fails"
        );
    }
}

async fn exercise_sessions(expected_provider: TransformersExecutionProvider) {
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
            .expect("prepare two sessions");
        let loaded = Arc::clone(
            lock_std_mutex(&model.state)
                .as_ref()
                .expect("prepared model"),
        );
        assert_eq!(
            loaded.sessions.provider(),
            expected_provider,
            "only the CPU-core scenario may fall back to CPU"
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
            let vectors = result.expect("inference").vectors;
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
        println!("{expected_provider:?} generation={generation} sessions=2 released");
    }
}
