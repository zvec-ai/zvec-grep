use super::*;
use std::{process::Command, time::Instant};

const CHILD: &str = "ZG_TEST_LLAMA_CUDA_SHUTDOWN_CHILD";
const COMPLETED: &str = "llama CUDA runtime shutdown completed";

#[test]
#[ignore = "requires CUDA and ZVEC_GREP_TEST_MODEL_CACHE with Gemma; run by Rust CUDA CI"]
fn cached_embeddinggemma_cuda_workers_exit_cleanly() {
    if let Ok(scenario) = env::var(CHILD) {
        exercise_child(&scenario);
        // End the process immediately after runtime teardown: passing the test
        // body alone cannot detect a worker racing CUDA's atexit handlers.
        println!("{COMPLETED}");
        std::process::exit(0);
    }
    let test = concat!(
        module_path!(),
        "::cached_embeddinggemma_cuda_workers_exit_cleanly"
    );
    let test = test.split_once("::").expect("crate prefix").1;
    for order in ["model-first", "runtime-first"] {
        for concurrency in [1, 4] {
            for perturb in [false, true] {
                let scenario = format!("{order}:{concurrency}");
                let log = tempfile::NamedTempFile::new().expect("child log");
                let mut command = Command::new(env::current_exe().expect("test executable"));
                command
                    .args([test, "--exact", "--ignored", "--nocapture"])
                    .env(CHILD, &scenario);
                // Both streams need a shared offset to preserve native evidence.
                let output = log.reopen().expect("combined log");
                command
                    .stdout(output.try_clone().expect("stdout"))
                    .stderr(output);
                if cfg!(target_os = "linux") && perturb {
                    command
                        .env("MALLOC_PERTURB_", "165")
                        .env("GLIBC_TUNABLES", "glibc.malloc.tcache_count=0");
                } else {
                    command
                        .env_remove("MALLOC_PERTURB_")
                        .env_remove("GLIBC_TUNABLES");
                }
                let mut child = command.spawn().expect("CUDA shutdown child");
                let started = Instant::now();
                let status = loop {
                    if let Some(status) = child.try_wait().expect("child status") {
                        break status;
                    }
                    if started.elapsed() > Duration::from_secs(60) {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("CUDA shutdown child timed out: {scenario}");
                    }
                    std::thread::sleep(Duration::from_millis(20));
                };
                let output = std::fs::read_to_string(log.path()).expect("child output");
                println!("scenario={scenario} perturb={perturb}\n{output}");
                assert!(status.success(), "CUDA shutdown failed: {status}");
                assert!(output.contains(COMPLETED), "child must exercise shutdown");
                assert!(
                    output.contains("offloaded 25/25 layers to GPU"),
                    "child must perform native CUDA inference"
                );
            }
        }
    }
}

fn exercise_child(scenario: &str) {
    init_llama_test_logs();
    let (order, concurrency) = scenario.split_once(':').expect("shutdown scenario");
    let concurrency: usize = concurrency.parse().expect("context count");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let model = runtime.block_on(async {
        let model = LlamaCppEmbeddingModel::new(
            entry("local/embeddinggemma-300m"),
            ModelConfig {
                cache_dir: Some(PathBuf::from(
                    env::var_os("ZVEC_GREP_TEST_MODEL_CACHE").expect("pinned Gemma cache"),
                )),
                device: Some(Device::Cuda),
                ..ModelConfig::default()
            },
            ModelComputeRuntime::shared(),
            test_resources(concurrency),
        );
        let inputs = (0..concurrency)
            .map(|index| {
                vec![Content::Text(format!(
                    "find telescope calibration code {index}"
                ))]
            })
            .collect::<Vec<_>>();
        let result = model
            .embed(
                &inputs,
                EmbeddingOptions {
                    execution_concurrency: concurrency,
                    ..EmbeddingOptions::default()
                },
            )
            .await
            .expect("CUDA embeddings");
        assert_eq!(result.vectors.len(), concurrency);
        assert!(model.state.lock().await.as_ref().expect("loaded model").gpu);
        model
    });
    match order {
        "model-first" => {
            drop(model);
            drop(runtime);
        }
        "runtime-first" => {
            drop(runtime);
            drop(model);
        }
        _ => panic!("unknown shutdown order"),
    }
}
