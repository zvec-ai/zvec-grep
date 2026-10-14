//! Keep the descriptor limit isolated from other tests and inherited native locks.
mod support;

use std::{collections::BTreeSet, fs, sync::atomic::Ordering};

use support::{
    EmbeddingServer, configure_remote_model, index_options, info_options, native_file_records,
};
use zg_engine::{
    ZvecGrep,
    api::context::{
        ContextOptions,
        options::{ContentKind, ContextRoute, ContextRouteMode},
    },
};

#[tokio::test]
async fn bulk_writes_reopen_with_limited_file_descriptors() -> Result<(), Box<dyn std::error::Error>>
{
    #[cfg(unix)]
    {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
        let maximum = getrlimit(Resource::Nofile).maximum;
        setrlimit(
            Resource::Nofile,
            Rlimit {
                current: Some(maximum.unwrap_or(512).min(512)),
                maximum,
            },
        )?;
    }

    let directory = tempfile::Builder::new().prefix("zg-bulk-").tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::create_dir(root.join("docs"))?;
    let mut expected = BTreeSet::new();
    for index in 0..513 {
        let path = std::path::PathBuf::from(format!("docs/{index}.txt"));
        fs::write(root.join(&path), "Orchard apples and pears.\n")?;
        expected.insert(path);
    }
    let server = EmbeddingServer::start()?;
    configure_remote_model(&root, server.address)?;
    let engine = ZvecGrep::new();
    let indexed = engine.index(index_options(&root)).await?;
    assert_eq!(indexed.files_added, expected.len());
    assert_eq!(indexed.files_failed, 0);
    assert_eq!(server.inputs.load(Ordering::Acquire), expected.len());
    assert!(server.requests.load(Ordering::Acquire) > 0);
    let info = engine.info(info_options(&root)).await?;
    assert_eq!(
        info.status.expect("index status").files_indexed,
        expected.len()
    );
    engine.close();
    assert_eq!(native_file_records(&info.index_path)?.len(), expected.len());

    let reopened = ZvecGrep::new();
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        let result = reopened
            .context(ContextOptions {
                root: Some(root.clone()),
                target_kind: Some(ContentKind::Text),
                routes: vec![ContextRoute {
                    mode,
                    query: "orchard".into(),
                }],
                limit: Some(expected.len()),
                prefer_symbol: false,
                auto_update: false,
                allow_remote: true,
                api_key: Some("local-test-key".into()),
                ..ContextOptions::default()
            })
            .await?;
        let actual = result
            .items
            .into_iter()
            .map(|item| item.relative_path)
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected, "reopened {mode:?} results");
    }
    reopened.close();
    Ok(())
}
