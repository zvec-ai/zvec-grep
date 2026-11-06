//! Manual real-model regression, separate from the performance-only shape suite.
use futures_util::future::join_all;

use super::{
    Arc, Config, Content, Duration, EmbeddingOptions, EmbeddingPrepareOptions, EmbeddingPurpose,
    Instant, ModelRuntimeLease, ModelRuntimeManager, Value, json, nanos, quantiles, support,
};
use crate::{
    domain::model::{Device, EmbeddingResult, ModelConfig},
    models::ModelRuntimeRequest,
};

fn acquire(
    config: &Config,
    manager: &ModelRuntimeManager,
    budget: usize,
) -> Arc<ModelRuntimeLease> {
    Arc::new(
        manager
            .acquire(ModelRuntimeRequest::new(
                &config.reference,
                ModelConfig {
                    cache_dir: Some(config.cache.clone()),
                    device: Some(config.device),
                    ..Default::default()
                },
                Some(budget),
            ))
            .expect("shared runtime lease"),
    )
}

fn assert_vectors(actual: &EmbeddingResult, reference: &EmbeddingResult) {
    assert_eq!(actual.truncated, reference.truncated);
    assert_eq!(actual.vectors.len(), reference.vectors.len());
    for (a, b) in actual.vectors.iter().zip(&reference.vectors) {
        assert_eq!(a.len(), b.len());
        assert!(
            a.iter()
                .zip(b)
                .all(|(x, y)| x.is_finite() && (x - y).abs() <= 1e-5)
        );
    }
}

async fn client(
    model: &ModelRuntimeLease,
    inputs: &[Vec<Content>],
    expected: &EmbeddingResult,
    origin: Instant,
    barrier: &tokio::sync::Barrier,
    worker: usize,
    budget: usize,
) -> Vec<Value> {
    barrier.wait().await;
    let mut rows = Vec::new();
    let minimum = if worker == 8 { 100 } else { 16 };
    let purpose = if worker == 8 {
        EmbeddingPurpose::Query
    } else {
        EmbeddingPurpose::Document
    };
    while origin.elapsed() < Duration::from_secs(15) || rows.len() < minimum {
        assert!(
            origin.elapsed() < Duration::from_secs(90),
            "mixed workload failed to make progress"
        );
        let started = Instant::now();
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            model.embed(
                inputs,
                EmbeddingOptions {
                    purpose,
                    ..Default::default()
                },
                None,
            ),
        )
        .await
        .expect("mixed request timeout")
        .expect("mixed request inference");
        let finished = Instant::now();
        assert_vectors(&output, expected);
        rows.push(json!({"worker":worker,"purpose":if worker == 8 {"query"} else {"document"},
            "requested_c":budget,"effective_c":model.execution_concurrency(),
            "started_ns":nanos(started.duration_since(origin)),"finished_ns":nanos(finished.duration_since(origin)),
            "latency_ns":nanos(finished.duration_since(started)),"vectors":output.vectors.len()}));
        if worker == 8 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    rows
}

fn summarize(rows: &[Value], purpose: &str, elapsed: f64) -> Value {
    let chosen: Vec<_> = rows.iter().filter(|r| r["purpose"] == purpose).collect();
    let vectors: u64 = chosen
        .iter()
        .map(|r| r["vectors"].as_u64().expect("vector count"))
        .sum();
    json!({"completed":chosen.len(),
        "vectors_per_second":f64::from(u32::try_from(vectors).expect("bounded sample count")) / elapsed,
        "call_latency":quantiles(chosen.iter().map(|r| r["latency_ns"].as_u64().expect("latency")).collect())})
}

struct Fixtures {
    documents: Vec<Vec<Content>>,
    questions: Vec<Vec<Content>>,
    reference_docs: EmbeddingResult,
    reference_query: EmbeddingResult,
}

async fn fixtures(document: &ModelRuntimeLease, query: &ModelRuntimeLease) -> Fixtures {
    document
        .prepare(EmbeddingPrepareOptions::default(), None)
        .await
        .expect("prepare pool");
    let documents: Vec<_> = [
        "search code ".repeat(128),
        "find configuration ".repeat(128),
        "embedding ".repeat(300),
        "query model ".repeat(128),
    ]
    .into_iter()
    .map(|s| vec![Content::Text(s)])
    .collect();
    let questions = vec![vec![Content::Text("find authentication middleware".into())]];
    let reference_docs = document
        .embed(
            &documents,
            EmbeddingOptions {
                purpose: EmbeddingPurpose::Document,
                fixed_batch: true,
                ..Default::default()
            },
            None,
        )
        .await
        .expect("reference documents");
    let reference_query = query
        .embed(
            &questions,
            EmbeddingOptions {
                purpose: EmbeddingPurpose::Query,
                ..Default::default()
            },
            None,
        )
        .await
        .expect("reference query");
    Fixtures {
        documents,
        questions,
        reference_docs,
        reference_query,
    }
}

async fn scenario(config: &Config, mixed_budgets: bool) -> Value {
    let manager = ModelRuntimeManager::new();
    let high = if config.device == Device::Cpu { 16 } else { 8 };
    let low = if mixed_budgets {
        if config.device == Device::Cpu { 8 } else { 1 }
    } else {
        high
    };
    let document = acquire(config, &manager, high);
    let other = acquire(config, &manager, low);
    let query = acquire(config, &manager, 1);
    let Fixtures {
        documents,
        questions,
        reference_docs,
        reference_query,
    } = fixtures(&document, &query).await;
    // Different operation budgets must reuse the very same runtime counters.
    assert_eq!(
        json!(document.execution_stats()),
        json!(other.execution_stats())
    );
    assert_eq!(
        json!(document.execution_stats()),
        json!(query.execution_stats())
    );
    let before = document.execution_stats().expect("ORT stats");
    let origin = Instant::now();
    let barrier = tokio::sync::Barrier::new(9);
    let tasks = (0..9).map(|worker| {
        let model = if worker == 8 {
            &query
        } else if worker < 4 {
            &document
        } else {
            &other
        };
        let (inputs, expected) = if worker == 8 {
            (&questions, &reference_query)
        } else {
            (&documents, &reference_docs)
        };
        client(
            model,
            inputs,
            expected,
            origin,
            &barrier,
            worker,
            if worker == 8 {
                1
            } else if worker < 4 {
                high
            } else {
                low
            },
        )
    });
    let groups = join_all(tasks).await;
    let elapsed = origin.elapsed().as_secs_f64();
    let rows: Vec<_> = groups.into_iter().flatten().collect();
    let after = document
        .execution_stats()
        .expect("ORT stats after mixed load");
    assert_eq!(before.session_count, after.session_count);
    assert_eq!(
        after.actual_provider,
        if config.device == Device::Cpu {
            "cpu"
        } else {
            "cuda"
        }
    );
    let changes = after.pool_reconfigurations - before.pool_reconfigurations;
    if config.device == Device::Cpu && mixed_budgets {
        assert!(
            changes > 0,
            "different CPU budgets must exercise pool reconfiguration"
        );
    } else {
        assert_eq!(changes, 0, "stable CPU/CUDA pool must not rebuild");
    }
    let result = json!({"mixed_budgets":mixed_budgets,"requested_document_c":[high,low],
        "elapsed_seconds":elapsed,"documents":summarize(&rows,"document",elapsed),"queries":summarize(&rows,"query",elapsed),
        "execution":support::execution_measurement(Some(before),Some(after)),"rows":rows});
    drop(document);
    drop(other);
    drop(query);
    manager.close();
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual MiniLM regression; pinned cache, release build and explicit CPU/CUDA device required"]
async fn cached_minilm_documents_and_queries() {
    #[expect(clippy::assertions_on_constants)]
    {
        assert!(!cfg!(debug_assertions), "use --release");
    }
    let config = Config::environment();
    assert_eq!(config.reference, "local/all-minilm-l6-v2");
    let provenance = support::verify_artifacts(config.entry, &config.cache).await;
    let steady = scenario(&config, false).await;
    let mixed = scenario(&config, true).await;
    println!(
        "ZG_MIXED_RESULT={}",
        json!({"protocol_pass":true,"provenance":provenance,
        "scope":"model-runtime document/query calls including admission; excludes indexing storage and retrieval; output checked against fixed-batch references",
        "document_clients":8,"query_clients":1,"query_pause_ms":25,
        "scenarios":[steady,mixed]})
    );
}
