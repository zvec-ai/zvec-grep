use super::*;
use crate::domain::{EntityMetadata, PdfBlock, PdfSourcePart, SourcePath};

#[tokio::test]
async fn pdf_conversion_honors_cancellation_before_native_work() {
    let models = crate::models::ModelRuntimeManager::new();
    let service = PdfExtractionService::new(
        models.clone(),
        crate::models::layout::LayoutModelConfig::default(),
    );
    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();
    let result = service
        .convert(
            SourcePath::new("cancelled.pdf").expect("valid test fixture"),
            Arc::from(&b"invalid PDF"[..]),
            signal,
            None,
        )
        .await;
    assert_eq!(
        result
            .expect_err("expected failure")
            .into_engine_error()
            .code(),
        EngineError::CANCELLED
    );
    assert_eq!(models.snapshot().cached_runtimes, 0);
}

#[tokio::test]
async fn pdf_corruption_does_not_download_or_load_models() {
    let models = crate::models::ModelRuntimeManager::new();
    let service = PdfExtractionService::new(
        models.clone(),
        crate::models::layout::LayoutModelConfig::default(),
    );
    let result = service
        .convert(
            SourcePath::new("invalid.pdf").expect("valid test fixture"),
            Arc::from(&b"invalid PDF"[..]),
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await;
    let error = result.expect_err("expected failure");
    assert!(!error.should_abort());
    assert_eq!(
        error.into_engine_error().code(),
        EngineError::INVALID_ARGUMENT
    );
    assert_eq!(models.snapshot().cached_runtimes, 0);
}

#[tokio::test]
#[ignore = "Requires packaged PDFium/ORT, PP-S download, and ZG_PDF_SAMPLE pointing to a native-text PDF; optional ZG_PDF_OUTPUT writes conversion artifacts"]
async fn pdf_native_layout_smoke() {
    let input = std::path::PathBuf::from(std::env::var_os("ZG_PDF_SAMPLE").expect("ZG_PDF_SAMPLE"));
    let models = crate::models::ModelRuntimeManager::new();
    let service = PdfExtractionService::new(
        models.clone(),
        crate::models::layout::LayoutModelConfig::default(),
    );
    let started = std::time::Instant::now();
    let result = service
        .convert(
            SourcePath::new("sample.pdf").expect("valid test fixture"),
            std::fs::read(input).expect("valid test fixture").into(),
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .expect("valid test fixture");
    let elapsed = started.elapsed();
    assert!(!result.text.text.is_empty());
    result
        .source_map
        .validate(&result.text.text)
        .expect("valid test fixture");
    let entities = extract(&result, ChunkOptions::default()).expect("valid test fixture");
    assert!(!entities.is_empty());
    if let Some(output) = std::env::var_os("ZG_PDF_OUTPUT") {
        let output = std::path::PathBuf::from(output);
        std::fs::create_dir_all(&output).expect("valid test fixture");
        std::fs::write(output.join("conversion.md"), &result.text.text)
            .expect("valid test fixture");
        std::fs::write(
            output.join("provenance.json"),
            serde_json::to_vec_pretty(&result.source_map).expect("valid test fixture"),
        )
        .expect("valid test fixture");
        std::fs::write(output.join("timing.json"), serde_json::to_vec_pretty(&serde_json::json!({"elapsed_ms": elapsed.as_secs_f64()*1000.0,"pages":result.source_map.pages(0..result.source_map.text_len),"entities":entities.len()})).expect("valid test fixture")).expect("valid test fixture");
    }
    eprintln!(
        "PDF conversion: {} entities, {:?}, {:.1} ms",
        entities.len(),
        result.source_map.pages(0..result.source_map.text_len),
        elapsed.as_secs_f64() * 1000.0
    );
    drop(service);
    models.close();
}

fn source(pieces: &[(u32, &str)]) -> PdfMarkdownSource {
    let mut text = String::new();
    let mut parts = Vec::new();
    for (index, (page, piece)) in pieces.iter().enumerate() {
        let start = text.len();
        text.push_str(piece);
        parts.push(PdfSourcePart {
            range: start..text.len(),
            block: PdfBlock {
                page: *page,
                id: u32::try_from(index + 1).expect("block ID"),
                bounds: Some([0, 0, 500_000, 500_000]),
            },
        });
    }
    PdfMarkdownSource {
        source_map: PdfSourceMap {
            markdown_start: 0,
            text_len: text.len(),
            parts,
        },
        text: TextSource {
            relative_path: SourcePath::new("manual.pdf").expect("path"),
            formats: vec![FileFormat::Pdf],
            text,
        },
    }
}

#[test]
fn pdf_markdown_reuses_sections_and_preserves_nonconsecutive_physical_pages() {
    let source = source(&[
        (2, "# Recovery\n\n等待 starts here and "),
        (4, "continues on another page.\n\n"),
        (5, "## Retry\n\n- Wait five minutes.\n- Try again.\n"),
    ]);
    let entities = extract(&source, ChunkOptions::default()).expect("extract");
    assert_eq!(entities.len(), 2);
    let Range::Pdf(first) = &entities[0].source_range else {
        panic!("PDF provenance");
    };
    assert_eq!(first.pages(0..first.text_len), [2, 4]);
    assert_eq!(first.parts.len(), 2);
    assert_eq!(first.parts[0].block.id, 1);
    assert_eq!(first.parts[1].block.id, 2);
    let Range::Pdf(second) = &entities[1].source_range else {
        panic!("PDF provenance");
    };
    assert_eq!(second.pages(0..second.text_len), [5]);
    assert_eq!(second.parts[0].range.start, 0);
    assert!(
        matches!(&entities[1].metadata, Some(EntityMetadata::Markdown(m))
        if m.heading.as_deref() == Some("Retry") && m.scope.as_deref() == Some("Recovery"))
    );
    assert!(matches!(&entities[0].content, Content::Text(text) if text.contains("and continues")));
}

#[test]
fn pdf_markdown_fragments_retain_exact_utf8_source_mapping() {
    let source = source(&[
        (1, "# Intro\n\n中文😀 alpha beta gamma\n"),
        (2, "delta epsilon zeta eta\n"),
    ]);
    let entities = extract(
        &source,
        ChunkOptions {
            max_chunk_chars: Some(20),
            chunk_overlap_chars: Some(4),
        },
    )
    .expect("extract");
    assert_eq!(entities.len(), 1);
    assert!(entities[0].fragments.len() > 1);
    let Content::Text(text) = &entities[0].content else {
        panic!("text");
    };
    let Range::Pdf(map) = &entities[0].source_range else {
        panic!("source map");
    };
    let second_start = text.find("delta").expect("second page");
    let selected = map
        .slice(second_start..text.len(), &text[second_start..])
        .expect("slice");
    assert_eq!(selected.pages(0..selected.text_len), [2]);
    for fragment in &entities[0].fragments {
        let Range::Byte(range) = &fragment.range else {
            continue;
        };
        let start = usize::try_from(range.start_offset()).expect("start");
        let end = usize::try_from(range.end_offset()).expect("end");
        map.slice(start..end, &text[start..end])
            .expect("valid fragment provenance");
    }
}

#[test]
fn pdf_markdown_rejects_missing_or_invalid_provenance() {
    let original = source(&[(1, "# Intro\n中文")]);
    for invalid in 0..4 {
        let mut source = original.clone();
        match invalid {
            0 => source.source_map.parts[0].block.page = 0,
            1 => source.source_map.parts[0].range.end -= 1,
            2 => source.source_map.parts[0].range.start = 2,
            _ => source.source_map.parts[0].block.bounds = Some([0, 0, 1_000_001, 10]),
        }
        assert!(extract(&source, ChunkOptions::default()).is_err());
    }
}
