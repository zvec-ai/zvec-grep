//! R2 fixtures from `INDEX_FAILURES_DIAGNOSIS.md`.
//!
//! Content that carries no text used to fail the whole file: the workspace
//! stayed `Failed` and the CLI exited non-zero on every run. Binary content is
//! now skipped, keeping the rest of the index published and ready. A BOM that
//! does not decode stays malformed text, so it still fails its own path and is
//! retried on the next pass.

#[allow(dead_code)] // Shared harness; each test binary uses a different subset.
mod support;

use std::fs;
use std::path::Path;

use support::{EmbeddingServer, configure_remote_model, index_options, info_options};
use zg_engine::{
    ZvecGrep,
    api::index::{IndexOptions, result::SkippedFileReason},
};

/// A PNG signature: invalid UTF-8, and C0 bytes that mark binary content.
const PNG_HEADER: [u8; 16] = [
    0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D', b'R',
];

/// A UTF-16 little-endian BOM followed by half a code unit.
const MALFORMED_BOM: [u8; 3] = [0xff, 0xfe, 0xff];

#[tokio::test]
async fn binary_content_is_skipped_while_a_malformed_bom_still_fails()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    fs::write(
        root.join("good.js"),
        "export function good() { return 1; }\n",
    )?;
    fs::write(root.join("binary.js"), PNG_HEADER)?;
    fs::write(root.join("bom.js"), MALFORMED_BOM)?;
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;

    let engine = ZvecGrep::new();
    let result = engine
        .index(IndexOptions {
            ..index_options(root)
        })
        .await?;
    engine.close();

    assert_eq!(
        result.files_failed, 1,
        "only malformed text may fail a path: {result:?}"
    );
    assert_eq!(result.failed_files.len(), 1, "{:?}", result.failed_files);
    assert_eq!(result.failed_files[0].path, Path::new("bom.js"));
    assert!(!result.failed_files[0].reason.is_empty());
    assert!(result.entities_created > 0, "the text file stays indexed");
    assert_eq!(result.skipped_counts.get("binary"), Some(&1));
    assert!(
        !result.skipped_counts.contains_key("malformed_bom"),
        "{:?}",
        result.skipped_counts
    );
    let reasons = result
        .skipped
        .iter()
        .map(|file| (file.path.clone(), file.reason))
        .collect::<Vec<_>>();
    assert!(
        reasons
            .iter()
            .any(|(path, reason)| path.ends_with("binary.js")
                && *reason == SkippedFileReason::Binary),
        "{reasons:?}"
    );
    assert!(
        reasons.iter().all(|(path, _)| !path.ends_with("bom.js")),
        "{reasons:?}"
    );

    // A failed path stays pending, so the retry reports it again while the
    // binary sample is rediscovered as a skip.
    let engine = ZvecGrep::new();
    let second = engine
        .index(IndexOptions {
            ..index_options(root)
        })
        .await?;
    engine.close();
    assert_eq!(second.files_failed, 1, "{second:?}");
    assert_eq!(second.failed_files[0].path, Path::new("bom.js"));
    assert_eq!(second.skipped_counts.get("binary"), Some(&1));

    Ok(())
}

#[tokio::test]
async fn a_file_that_turns_binary_loses_its_entities_and_stays_skipped()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    fs::write(
        root.join("source.js"),
        "export function work() { return 2; }\n",
    )?;
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;

    let engine = ZvecGrep::new();
    let first = engine
        .index(IndexOptions {
            ..index_options(root)
        })
        .await?;
    assert_eq!(first.files_failed, 0, "{first:?}");
    assert!(first.entities_created > 0);

    fs::write(root.join("source.js"), PNG_HEADER)?;
    let second = engine
        .index(IndexOptions {
            ..index_options(root)
        })
        .await?;
    assert_eq!(second.files_failed, 0, "{second:?}");
    assert_eq!(second.skipped_counts.get("binary"), Some(&1));

    // The skipped path kept no snapshot, so its old entities cannot be served.
    let info = engine.info(info_options(root)).await?;
    let status = info.status.expect("status was requested");
    assert_eq!(status.files_indexed, 0, "{status:?}");
    assert_eq!(status.entities_indexed, 0, "{status:?}");
    engine.close();

    Ok(())
}
