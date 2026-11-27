//! R1 empty-reference-name fixtures from `INDEX_FAILURES_DIAGNOSIS.md`.
//!
//! Every case below indexes cleanly once the graph walk stops deriving a name
//! from a zero-width tree-sitter node or from an empty module specifier. The
//! walk leaves a zero-width `type_identifier` or `identifier` unreported, and
//! the import wrapper drops a specifier that spells nothing, so none of these
//! queue a reference spelled `""`. The text-only case proves the harness itself
//! is sound: the same constructions index fine without graph collection, which
//! is why these failures belong to the graph feature rather than to extraction
//! in general.

#[allow(dead_code)] // Shared harness; each test binary uses a different subset.
mod support;

use std::fs;

use support::{EmbeddingServer, configure_remote_model, index_options};
use zg_engine::{
    ZvecGrep,
    api::index::{IndexOptions, IndexResult},
};

/// Indexes one file in its own workspace with graph collection enabled.
async fn index_with_graph(
    name: &str,
    source: &str,
) -> Result<(IndexResult, EmbeddingServer), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    fs::write(root.join(name), source)?;
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    let engine = ZvecGrep::new();
    let result = engine
        .index(IndexOptions {
            // The shared fixture manifest pins graph indexing off, and the
            // setting may only change through a rebuild.
            graph_indexing: Some(true),
            rebuild: true,
            ..index_options(root)
        })
        .await;
    engine.close();
    // Keep the fixture alive until the index result is read.
    Ok((result?, server))
}

/// Indexes the same source with graph collection off.
async fn index_without_graph(
    name: &str,
    source: &str,
) -> Result<IndexResult, Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    fs::write(root.join(name), source)?;
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    let engine = ZvecGrep::new();
    let result = engine
        .index(IndexOptions {
            graph_indexing: Some(false),
            ..index_options(root)
        })
        .await;
    engine.close();
    Ok(result?)
}

fn assert_indexed(result: &IndexResult) {
    assert_eq!(
        (result.files_added, result.files_failed),
        (1, 0),
        "a single-file workspace must index without failures: {result:?}"
    );
}

#[tokio::test]
async fn control_empty_brace_initializer_indexes_without_graph()
-> Result<(), Box<dyn std::error::Error>> {
    // Proves the fixture itself is indexable; the graph case below fails only
    // because walk-time collection reads the missing type identifier as a name.
    let result = index_without_graph("sample.h", "void f(int x = {});\n").await?;
    assert_indexed(&result);
    Ok(())
}

#[tokio::test]
async fn empty_brace_initializer_keeps_the_file_indexable_under_graph()
-> Result<(), Box<dyn std::error::Error>> {
    let (result, _server) = index_with_graph("sample.h", "void f(int x = {});\n").await?;
    assert_indexed(&result);
    Ok(())
}

#[tokio::test]
async fn macro_prefixed_class_member_enum_keeps_the_file_indexable_under_graph()
-> Result<(), Box<dyn std::error::Error>> {
    let (result, _server) = index_with_graph(
        "sample.h",
        "class GuiExport Camera { public: enum O { Top }; };\n",
    )
    .await?;
    assert_indexed(&result);
    Ok(())
}

#[tokio::test]
async fn macro_prefixed_nested_class_forward_keeps_the_file_indexable_under_graph()
-> Result<(), Box<dyn std::error::Error>> {
    let (result, _server) = index_with_graph(
        "sample.h",
        "class MeshExport KDTree { public: class Private; };\n",
    )
    .await?;
    assert_indexed(&result);
    Ok(())
}

#[tokio::test]
async fn empty_require_module_keeps_the_file_indexable_under_graph()
-> Result<(), Box<dyn std::error::Error>> {
    let (result, _server) = index_with_graph("sample.js", "const A = require(\"\");\n").await?;
    assert_indexed(&result);
    Ok(())
}
