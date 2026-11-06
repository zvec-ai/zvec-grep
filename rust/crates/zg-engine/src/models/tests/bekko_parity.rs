//! Full-vector differential check, invoked by the existing real Bekko CI smoke.
//! Optional `ZVEC_GREP_BEKKO_PARITY_INPUTS` supplies a JSON string array of local
//! corpus samples; CI always exercises generated inputs and long boundaries.

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

fn reference_model() -> TransformersEmbeddingModel {
    let EmbeddingCatalogEntry::StaticInt8(entry) =
        get_embedding_model_catalog_entry("local/bekko-256-int8").expect("Bekko")
    else {
        panic!("native static backend");
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
    println!("BEKKO_ONNX_PARITY_OK vectors={vectors} dimensions=256 comparison=bitwise");
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
    let expected = onnx.embed(inputs, options).await.expect("ONNX reference");
    assert_eq!(
        actual.truncated, expected.truncated,
        "truncation must match"
    );
    assert_eq!(actual.vectors.len(), expected.vectors.len());
    for (row, (a, b)) in actual.vectors.iter().zip(&expected.vectors).enumerate() {
        assert_eq!(a.len(), 256);
        assert_eq!(b.len(), 256);
        for (column, (&a, &b)) in a.iter().zip(b).enumerate() {
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "row={row} column={column} native={a} ONNX={b}"
            );
        }
    }
}
