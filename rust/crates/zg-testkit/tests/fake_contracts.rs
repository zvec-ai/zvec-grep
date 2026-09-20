use std::path::PathBuf;

use tempfile::tempdir;
use zg_engine::{EngineError, ZvecGrep, api::context::ContextOptions};
use zg_testkit::load_cli_case;

#[tokio::test]
async fn zvec_grep_executes_typed_requests_directly() {
    let temporary = tempdir().expect("temporary workspace should be created");
    let first_root = temporary.path().join("first");
    let second_root = temporary.path().join("second");
    std::fs::create_dir_all(&first_root).expect("first workspace should be created");
    std::fs::create_dir_all(&second_root).expect("second workspace should be created");
    std::fs::write(first_root.join("fixture.txt"), "first needle\n")
        .expect("first fixture should be written");
    std::fs::write(second_root.join("fixture.txt"), "second needle\n")
        .expect("second fixture should be written");
    let service = ZvecGrep::new();

    let first_reply = service
        .context(ContextOptions {
            root: Some(first_root.clone()),
            query: Some("needle".to_owned()),
            rg: true,
            ..ContextOptions::default()
        })
        .await
        .expect("first lexical request should complete");
    let second_reply = service
        .context(ContextOptions {
            root: Some(second_root.clone()),
            query: Some("needle".to_owned()),
            rg: true,
            ..ContextOptions::default()
        })
        .await
        .expect("second lexical request should complete");

    assert_eq!(first_reply.root, first_root);
    assert_eq!(first_reply.items.len(), 1);
    assert_eq!(first_reply.items[0].content, "first needle");
    assert_eq!(second_reply.root, second_root);
    assert_eq!(second_reply.items.len(), 1);
    assert_eq!(second_reply.items[0].content, "second needle");

    service.close();
    let error = service
        .context(ContextOptions {
            root: Some(temporary.path().to_path_buf()),
            query: Some("needle".to_owned()),
            rg: true,
            ..ContextOptions::default()
        })
        .await
        .expect_err("closed service must reject requests");
    assert_eq!(error.code(), EngineError::RESOURCE_CLOSED);
    assert!(error.reported_at().is_some());
}

#[test]
fn compatibility_fixture_schema_is_loadable() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../compat/cli/managed-rg-no-match.json");
    let fixture = load_cli_case(&path).expect("fixture should be valid");
    assert_eq!(fixture.schema_version, 1);
    assert_eq!(fixture.expected.exit_code, 0);
}
