//! R2 fixtures from `INDEX_FAILURES_DIAGNOSIS.md`.
//!
//! Content that carries no text used to fail the whole file: the workspace
//! stayed `Failed` and the CLI exited non-zero on every run. It is now skipped,
//! keeping the rest of the index published and ready. The two causes stay
//! distinguishable so the counters say which one a path hit.

#[allow(dead_code)] // Shared harness; each test binary uses a different subset.
mod support;

use std::fs;

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
async fn binary_and_malformed_bom_content_are_skipped_not_failed()
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

    assert_eq!(result.files_failed, 0, "no path may fail: {result:?}");
    assert!(result.failed_files.is_empty(), "{:?}", result.failed_files);
    assert!(result.entities_created > 0, "the text file stays indexed");
    assert_eq!(result.skipped_counts.get("binary"), Some(&1));
    assert_eq!(result.skipped_counts.get("malformed_bom"), Some(&1));
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
        reasons
            .iter()
            .any(|(path, reason)| path.ends_with("bom.js")
                && *reason == SkippedFileReason::MalformedBom),
        "{reasons:?}"
    );

    // A second pass rediscovers the same skips instead of retrying a failure.
    let engine = ZvecGrep::new();
    let second = engine
        .index(IndexOptions {
            ..index_options(root)
        })
        .await?;
    engine.close();
    assert_eq!(second.files_failed, 0, "{second:?}");
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
