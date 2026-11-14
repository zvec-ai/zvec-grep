use std::{fs, path::Path};

use super::*;
use crate::api::index::options::ScanRulesUpdate;

fn write(root: &Path, path: &str, bytes: impl AsRef<[u8]>) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().expect("parent")).expect("directories");
    fs::write(path, bytes).expect("fixture");
}

#[test]
fn scene_thresholds_and_ties_use_weights_not_file_counts() {
    for (code, text, scene, model) in [
        (60, 40, EmbeddingScene::Code, CODE_MODEL),
        (40, 60, EmbeddingScene::Text, TEXT_MODEL),
        (49, 51, EmbeddingScene::Mixed, TEXT_MODEL),
        (51, 49, EmbeddingScene::Mixed, CODE_MODEL),
        (50, 50, EmbeddingScene::Mixed, CODE_MODEL),
        (0, 0, EmbeddingScene::Unknown, CODE_MODEL),
        (u64::MAX, u64::MAX, EmbeddingScene::Mixed, CODE_MODEL),
    ] {
        let analysis = summarize(
            EmbeddingEvidence {
                files: 100,
                weight_bytes: code,
            },
            EmbeddingEvidence {
                files: 1,
                weight_bytes: text,
            },
            500,
        );
        assert_eq!(analysis.scene, scene);
        assert_eq!(analysis.recommended_model, model);
        assert_eq!(analysis.other_files, 500);
    }
}

#[tokio::test]
async fn oversized_single_file_cannot_dominate_the_recommendation() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    write(root, "large.rs", vec![b' '; 512 * 1024]);
    write(root, "one.md", vec![b' '; 48 * 1024]);
    write(root, "two.md", vec![b' '; 48 * 1024]);
    write(root, "config.json", vec![b' '; 1024 * 1024]);
    let analysis = analyze(root, &ScanRules::default(), None)
        .await
        .expect("analysis");
    assert_eq!(
        analysis.code,
        EmbeddingEvidence {
            files: 1,
            weight_bytes: FILE_WEIGHT_LIMIT
        }
    );
    assert_eq!(
        analysis.text,
        EmbeddingEvidence {
            files: 2,
            weight_bytes: 96 * 1024
        }
    );
    assert_eq!(analysis.other_files, 1);
    assert_eq!(analysis.scene, EmbeddingScene::Text);
    assert_eq!(analysis.recommended_model, TEXT_MODEL);
}

#[tokio::test]
async fn analysis_obeys_index_scan_selection_and_size_limits() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    write(root, "src/main.rs", "fn main() {}\n");
    write(root, "docs/readme.md", "document");
    write(root, "docs/ignored.md", "ignored");
    write(root, "docs/large.md", vec![b' '; 100]);
    write(root, "docs/.hidden.md", "hidden");
    write(root, "vendor/dependency.md", "dependency");
    write(root, ".zvec-grep/internal.md", "metadata");
    write(root, ".gitignore", "docs/ignored.md\n");
    let rules = ScanRulesUpdate {
        included_formats: Some(vec![FileFormat::Markdown]),
        max_file_size_bytes: Some(Some(20)),
        ..Default::default()
    }
    .apply(&ScanRules::default())
    .expect("scan rules");
    let analysis = analyze(root, &rules, None).await.expect("analysis");
    assert_eq!(analysis.text.files, 1);
    assert_eq!(analysis.text.weight_bytes, 8);
    assert_eq!(analysis.code.files, 0);
    assert_eq!(analysis.recommended_model, TEXT_MODEL);
}

#[tokio::test]
async fn formats_share_index_detection_and_do_not_count_ambiguous_files_twice() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    write(root, "header.h", "int value;\n");
    write(root, "script", "#!/usr/bin/env python\nprint('example')\n");
    write(root, "notes.unknown", "ordinary text\n");
    write(root, "document.mdx", "```rust\nfn main() {}\n```\n");
    write(root, "page.html", "<p>example</p>");
    write(root, "config.yaml", "key: value\n");
    write(root, "image.png", [1, 2, 3, 4]);
    write(root, "binary.unknown", [0, 1, 2, 3]);
    write(root, "empty.md", "");
    write(root, "oversized.rs", vec![b' '; 1024 * 1024 + 1]);
    let analysis = analyze(root, &ScanRules::default(), None)
        .await
        .expect("analysis");
    assert_eq!(analysis.code.files, 2);
    assert_eq!(analysis.text.files, 2);
    assert_eq!(analysis.other_files, 2);
}

#[tokio::test]
async fn analysis_propagates_cancellation_and_invalid_scan_rules() {
    let directory = tempfile::tempdir().expect("workspace");
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = analyze(directory.path(), &ScanRules::default(), Some(cancelled))
        .await
        .expect_err("cancelled before discovery");
    assert_eq!(error.code(), EngineError::CANCELLED);
    let rules = ScanRulesUpdate {
        ignore_files: Some(vec![directory.path().to_path_buf()]),
        ..Default::default()
    }
    .apply(&ScanRules::default())
    .expect("rules");
    assert!(analyze(directory.path(), &rules, None).await.is_err());
}

#[test]
fn configured_choices_keep_precedence_and_validate_environment_overrides() {
    let explicit = choose_configured(
        Some(TEXT_MODEL.into()),
        Some(CODE_MODEL.into()),
        "invalid/model".into(),
        None,
    )
    .expect("explicit ignores defaults")
    .expect("choice");
    assert_eq!(explicit.source, EmbeddingSelectionSource::Explicit);
    assert_eq!(explicit.model, TEXT_MODEL);
    let saved = choose_configured(
        None,
        Some(CODE_MODEL.into()),
        TEXT_MODEL.into(),
        Some(TEXT_MODEL.into()),
    )
    .expect("saved model")
    .expect("choice");
    assert_eq!(saved.source, EmbeddingSelectionSource::ExistingIndex);
    assert_eq!(saved.model, CODE_MODEL);
    let environment = choose_configured(
        None,
        None,
        format!(" {TEXT_MODEL} "),
        Some(CODE_MODEL.into()),
    )
    .expect("environment")
    .expect("choice");
    assert_eq!(environment.source, EmbeddingSelectionSource::Environment);
    assert_eq!(environment.model, TEXT_MODEL);
    let configured = choose_configured(None, None, " ".into(), Some(TEXT_MODEL.into()))
        .expect("configuration")
        .expect("choice");
    assert_eq!(configured.source, EmbeddingSelectionSource::Configuration);
    assert!(
        choose_configured(None, None, String::new(), None)
            .expect("automatic")
            .is_none()
    );
    assert!(choose_configured(None, None, "invalid/model".into(), None).is_err());
}
