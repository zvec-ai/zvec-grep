//! Full-vector differential check, invoked by the existing real Bekko CI smoke.
//! Optional `ZVEC_GREP_BEKKO_PARITY_INPUTS` supplies a JSON string array of local
//! corpus samples; CI always exercises generated inputs and long boundaries.
//! Single-input ONNX is the bitwise oracle. Batched ONNX can select a different
//! FP32 reduction order on x86, so it receives a separate numerical check.

use std::env;

use crate::{
    domain::{
        Content,
        model::{Device, EmbeddingPurpose, ModelConfig},
    },
    models::{
        backends::TransformersEmbeddingModel,
        catalog::{EmbeddingCatalogEntry, TransformersConfig, get_embedding_model_catalog_entry},
        runtime::ModelComputeRuntime,
        spi::{EmbeddingModel, EmbeddingOptions, EmbeddingPrepareOptions},
    },
};

const DIMENSION: usize = 256;
// The pinned ORT 1.21 Linux diagnostic measured a maximum component error of
// 7.734e-6 and a minimum cosine similarity of 0.9999999996065 across 560 vectors.
// These bounds apply only to batched ONNX, never to native batch invariance or
// the single-input ONNX reference.
const MAX_BATCH_ABSOLUTE_ERROR: f32 = 1e-5;
const MIN_BATCH_COSINE: f64 = 1.0 - 1e-8;

fn reference_model() -> TransformersEmbeddingModel {
    let EmbeddingCatalogEntry::Bekko(entry) =
        get_embedding_model_catalog_entry("local/bekko-256-int8").expect("Bekko")
    else {
        panic!("native Bekko backend");
    };
    TransformersEmbeddingModel::new(
        TransformersConfig {
            reference: entry.reference,
            provider: entry.provider,
            model: entry.model,
            repo: entry.download.hugging_face.repo,
            revision: entry.download.hugging_face.revision,
            download: entry.download,
            dtype: "int8",
            add_special_tokens: false,
            dimension: entry.dimension,
            metric: entry.metric,
            pooling: "mean",
            normalize: true,
            query_prefix: None,
            document_prefix: None,
            max_input_tokens: entry.max_input_tokens,
            max_batch_size: entry.max_batch_size,
        },
        ModelConfig {
            cache_dir: env::var_os("ZVEC_GREP_MODEL_CACHE").map(Into::into),
            device: Some(Device::Cpu),
            ..ModelConfig::default()
        },
        ModelComputeRuntime::shared(),
    )
}

pub(super) async fn assert_onnx_parity(native: &dyn EmbeddingModel) {
    let onnx = reference_model();
    onnx.prepare(EmbeddingPrepareOptions::default())
        .await
        .expect("load pinned ONNX oracle");
    for text in ["", " \n\t"] {
        let inputs = [vec![Content::Text(text.into())]];
        let actual = native
            .embed(&inputs, EmbeddingOptions::default())
            .await
            .expect_err("empty input rejected");
        let expected = onnx
            .embed(&inputs, EmbeddingOptions::default())
            .await
            .expect_err("empty input rejected");
        assert_eq!(actual.code(), expected.code());
    }
    let mut texts = [
        "\0",
        "\u{200b}",
        "find authentication middleware",
        "长春光华学院在哪",
        "日本で一番高い山は？",
        "Café CAFÉ cafe",
        "fn main() { println!(\"hello\"); }",
        "🦄🚀",
        "hello 🦄",
        "文字\u{2ffff}",
        "[UNK] [PAD]",
        "\0\u{200b}\u{feff}",
        "hello hello hello",
        "hello world world hello",
        "world hello hello world",
    ]
    .map(str::to_owned)
    .to_vec();
    for length in [1, 7, 31, 128, 511, 1024] {
        for text in [
            "search store read update check process share return ",
            "数据库 检索 向量 测试 ",
            "日本語の文章を検索します。",
        ] {
            texts.push(text.repeat(length));
        }
    }
    if let Some(path) = env::var_os("ZVEC_GREP_BEKKO_PARITY_INPUTS") {
        let extra: Vec<String> =
            serde_json::from_slice(&std::fs::read(path).expect("local parity inputs"))
                .expect("string array");
        texts.extend(extra);
    }
    let mut vectors = 0;
    for purpose in [EmbeddingPurpose::Query, EmbeddingPurpose::Document] {
        for batch in texts.chunks(native.info().model().max_batch_size()) {
            let inputs = batch
                .iter()
                .map(|text| vec![Content::Text(text.clone())])
                .collect::<Vec<_>>();
            compare(native, &onnx, &inputs, purpose).await;
            vectors += inputs.len();
        }
        // Single-item batches, all-empty batches, and combined content exercise
        // independence from padding, special-token and formatting differences.
        for inputs in [
            vec![vec![Content::Text("hello 🦄".into())]],
            vec![vec![Content::Text("\0".into())]; 2],
            vec![vec![
                Content::Text("hello".into()),
                Content::Code("world".into()),
            ]],
        ] {
            compare(native, &onnx, &inputs, purpose).await;
            vectors += inputs.len();
        }
        for count in [32_767, 32_768, 32_769] {
            let inputs = [vec![Content::Text(vec!["hello"; count].join(" "))]];
            compare(native, &onnx, &inputs, purpose).await;
            let actual = native
                .embed(
                    &inputs,
                    EmbeddingOptions {
                        purpose,
                        ..EmbeddingOptions::default()
                    },
                )
                .await
                .expect("boundary");
            assert_eq!(
                actual.truncated,
                if count > 32_768 { vec![0] } else { vec![] }
            );
            vectors += 1;
        }
        vectors += compare_concurrent_batches(native, &onnx, purpose).await;
    }
    println!(
        "BEKKO_ONNX_PARITY_OK vectors={vectors} dimensions={DIMENSION} \
         reference=onnx_single comparison=bitwise native_batch=bitwise \
         onnx_batch_max_abs={MAX_BATCH_ABSOLUTE_ERROR} onnx_batch_min_cosine={MIN_BATCH_COSINE}"
    );
}

async fn compare_concurrent_batches(
    native: &dyn EmbeddingModel,
    onnx: &dyn EmbeddingModel,
    purpose: EmbeddingPurpose,
) -> usize {
    // Several native batches share the compute pool. Check every component
    // against ONNX while work stealing can interleave their text rows.
    let concurrent_inputs = [
        vec![Content::Text("hello 🦄".into())],
        vec![Content::Text("数据库 检索 向量 测试 ".repeat(128))],
        vec![Content::Text("\0".into())],
        vec![Content::Text("日本語の文章を検索します。".repeat(64))],
    ];
    futures_util::future::join_all(
        (0..4).map(|_| compare(native, onnx, &concurrent_inputs, purpose)),
    )
    .await;
    4 * concurrent_inputs.len()
}

async fn compare(
    native: &dyn EmbeddingModel,
    onnx: &dyn EmbeddingModel,
    inputs: &[Vec<Content>],
    purpose: EmbeddingPurpose,
) {
    let options = EmbeddingOptions {
        purpose,
        ..EmbeddingOptions::default()
    };
    let actual = native
        .embed(inputs, options.clone())
        .await
        .expect("native embedding");
    let expected = onnx
        .embed(inputs, options.clone())
        .await
        .expect("batched ONNX reference");
    assert_eq!(
        actual.truncated, expected.truncated,
        "truncation must match"
    );
    assert_eq!(actual.vectors.len(), expected.vectors.len());
    assert_eq!(actual.vectors.len(), inputs.len());
    for (row, input) in inputs.iter().enumerate() {
        let singleton = std::slice::from_ref(input);
        // ORT's ReduceSum chooses its fast path using the batch shape and
        // thread-pool size. Single-input inference fixes the reduction order
        // without changing the production backend or its runtime settings.
        let single_onnx = onnx
            .embed(singleton, options.clone())
            .await
            .expect("single-input ONNX reference");
        let single_native = native
            .embed(singleton, options.clone())
            .await
            .expect("single-input native embedding");
        assert_eq!(single_onnx.vectors.len(), 1);
        assert_eq!(single_native.vectors.len(), 1);
        let truncated = if actual.truncated.contains(&row) {
            vec![0]
        } else {
            vec![]
        };
        assert_eq!(single_onnx.truncated, truncated);
        assert_eq!(single_native.truncated, truncated);
        assert_bitwise(&actual.vectors[row], &single_onnx.vectors[0], row);
        assert_bitwise(&actual.vectors[row], &single_native.vectors[0], row);
        assert_batched_equivalent(&actual.vectors[row], &expected.vectors[row], row);
    }
}

fn assert_bitwise(actual: &[f32], expected: &[f32], row: usize) {
    assert_eq!(actual.len(), DIMENSION);
    assert_eq!(expected.len(), DIMENSION);
    for (column, (&a, &b)) in actual.iter().zip(expected).enumerate() {
        assert!(a.is_finite() && b.is_finite(), "non-finite embedding");
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "row={row} column={column} native={a} reference={b}"
        );
    }
}

fn assert_batched_equivalent(actual: &[f32], expected: &[f32], row: usize) {
    assert_eq!(actual.len(), DIMENSION);
    assert_eq!(expected.len(), DIMENSION);
    let (mut dot, mut actual_norm, mut expected_norm) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (column, (&a, &b)) in actual.iter().zip(expected).enumerate() {
        assert!(a.is_finite() && b.is_finite(), "non-finite embedding");
        assert!(
            (a - b).abs() <= MAX_BATCH_ABSOLUTE_ERROR,
            "batched ONNX component drift: row={row} column={column} native={a} ONNX={b}"
        );
        dot += f64::from(a) * f64::from(b);
        actual_norm += f64::from(a) * f64::from(a);
        expected_norm += f64::from(b) * f64::from(b);
    }
    if actual_norm == 0.0 || expected_norm == 0.0 {
        assert_bitwise(actual, expected, row);
    } else {
        let cosine = dot / (actual_norm * expected_norm).sqrt();
        assert!(
            cosine >= MIN_BATCH_COSINE,
            "batched ONNX direction drift: row={row} cosine={cosine}"
        );
    }
}

#[cfg(test)]
#[path = "bekko_parity/tests.rs"]
mod tests;
