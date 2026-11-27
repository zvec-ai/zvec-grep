//!
//! Content that carries no text used to fail the whole file: the workspace
//! stayed `Failed` and the CLI exited non-zero on every run. Binary content is
//! now skipped, keeping the rest of the index published and ready. A BOM that
//! does not decode stays malformed text, so it still fails its own path and is
//! retried on the next pass.
//!
//! A skipped path keeps the snapshot its bytes produced, so status and diffs
//! answer for it: unchanged binary content is neither counted as a new file on
//! every run nor left `Stale`, and rewriting the path as text indexes it again.

#[allow(dead_code)] // Shared harness; each test binary uses a different subset.
mod support;

use std::fs;
use std::path::Path;

use support::{EmbeddingServer, configure_remote_model, index_options, info_options};
use zg_engine::{
    ZvecGrep,
    api::{
        index::{IndexOptions, result::SkippedFileReason},
        info::result::IndexStatus,
    },
};

/// A skip count map names every reason, so only the sum says "nothing skipped".
fn skip_total(counts: &std::collections::BTreeMap<String, usize>) -> usize {
    counts.values().sum()
}

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

    // A failed path stays pending, so the retry reports it again. The binary
    // path kept its record, so this run does not rediscover it as a new file.
    let engine = ZvecGrep::new();
    let second = engine
        .index(IndexOptions {
            ..index_options(root)
        })
        .await?;
    assert_eq!(second.files_failed, 1, "{second:?}");
    assert_eq!(second.failed_files[0].path, Path::new("bom.js"));
    assert_eq!(second.files_added, 0, "{second:?}");
    assert!(second.skipped.is_empty(), "{second:?}");
    assert_eq!(skip_total(&second.skipped_counts), 0, "{second:?}");

    let info = engine.info(info_options(root)).await?;
    let status = info.status.as_ref().expect("status was requested");
    assert_eq!(status.files_added, 0, "{status:?}");
    assert_eq!(status.files_pending, 1, "{status:?}");
    assert_eq!(status.files_indexed, 1, "{status:?}");
    assert_eq!(status.files_stored, 3, "{status:?}");
    assert_eq!(
        info.index_status(),
        IndexStatus::Failed,
        "only the malformed BOM keeps the workspace unready: {status:?}"
    );
    engine.close();

    Ok(())
}

#[tokio::test]
async fn a_file_that_turns_binary_leaves_the_index_and_keeps_the_workspace_ready()
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
    let indexed = engine.info(info_options(root)).await?;
    assert_eq!(indexed.index_status(), IndexStatus::Ready);
    assert_eq!(
        indexed.status.expect("status was requested").files_indexed,
        1
    );

    // The same path now holds binary content: its entities leave the index and
    // the skip is reported by the pass that read the new bytes.
    fs::write(root.join("source.js"), PNG_HEADER)?;
    let second = engine
        .index(IndexOptions {
            ..index_options(root)
        })
        .await?;
    assert_eq!(second.files_failed, 0, "{second:?}");
    assert_eq!(second.files_modified, 1, "{second:?}");
    assert_eq!(second.skipped_counts.get("binary"), Some(&1));

    let after = engine.info(info_options(root)).await?;
    let status = after.status.as_ref().expect("status was requested");
    assert_eq!(status.files_indexed, 0, "{status:?}");
    assert_eq!(status.entities_indexed, 0, "{status:?}");
    assert_eq!(status.files_added, 0, "{status:?}");
    assert_eq!(status.files_pending, 0, "{status:?}");
    assert_eq!(
        after.index_status(),
        IndexStatus::Ready,
        "a remembered skip must not leave the index stale: {status:?}"
    );

    // The stored snapshot answers for the unchanged bytes, so a later run has
    // nothing to rediscover.
    let third = engine
        .index(IndexOptions {
            ..index_options(root)
        })
        .await?;
    assert_eq!(third.files_unchanged, 1, "{third:?}");
    assert!(third.skipped.is_empty(), "{third:?}");
    assert_eq!(skip_total(&third.skipped_counts), 0, "{third:?}");

    // Rewriting the same path as text takes it back into the index.
    fs::write(
        root.join("source.js"),
        "export function work() { return 3; }\n",
    )?;
    let fourth = engine
        .index(IndexOptions {
            ..index_options(root)
        })
        .await?;
    assert_eq!(fourth.files_failed, 0, "{fourth:?}");
    assert!(fourth.entities_created > 0, "{fourth:?}");

    let restored = engine.info(info_options(root)).await?;
    let status = restored.status.as_ref().expect("status was requested");
    assert_eq!(status.files_indexed, 1, "{status:?}");
    assert!(status.entities_indexed > 0, "{status:?}");
    assert_eq!(restored.index_status(), IndexStatus::Ready, "{status:?}");
    engine.close();

    Ok(())
}
