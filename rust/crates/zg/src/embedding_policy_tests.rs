use super::*;
use zg_engine::api::index::{
    IndexOptions,
    embedding::EmbeddingSelectionSource,
    options::{Device, EmbeddingModelSpec},
};

const CODE: &str = "local/potion-code-16m-v2";
const TEXT: &str = "local/bekko-256-int8";

#[tokio::test]
async fn automatic_choice_preserves_an_index_published_after_preflight() {
    if std::env::var_os("ZG_TEST_EMBEDDING_ADMISSION").is_none() {
        let home = tempfile::tempdir().expect("isolated configuration");
        let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("ZVEC_GREP_") {
                command.env_remove(key);
            }
        }
        let output = command
            .args([
                "--exact",
                "embedding_policy_tests::automatic_choice_preserves_an_index_published_after_preflight",
                "--nocapture",
            ])
            .env("ZG_TEST_EMBEDDING_ADMISSION", "1")
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .output()
            .expect("isolated test");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    for rebuild in [false, true] {
        let root = tempfile::tempdir().expect("workspace");
        std::fs::write(root.path().join("guide.md"), "\n".repeat(1024)).expect("document");
        let engine = ZvecGrep::new();
        let mut queued = IndexOptions {
            root: Some(root.path().to_owned()),
            rebuild,
            ..Default::default()
        };
        resolve_cli_index_embedding(&mut queued)
            .await
            .expect("preflight");
        let selection = queued
            .embedding_default
            .as_ref()
            .expect("implicit selection");
        assert_eq!(selection.model, TEXT);
        assert_eq!(selection.source, EmbeddingSelectionSource::Automatic);

        // Publish another writer's explicit choice after the caller's preflight,
        // before the queued operation reaches writer admission.
        engine
            .index(IndexOptions {
                root: queued.root.clone(),
                embedding: Some(EmbeddingModelSpec {
                    reference: CODE.into(),
                    revision: None,
                    cache_dir: None,
                    endpoint: None,
                    device: Device::Auto,
                }),
                ..Default::default()
            })
            .await
            .expect("concurrent writer publishes Potion");
        engine
            .index(queued.clone())
            .await
            .expect("implicit indexing must reuse the newly published model");
        let saved = engine
            .resolve_index_embedding(IndexOptions {
                root: queued.root,
                ..Default::default()
            })
            .await
            .expect("saved model");
        assert_eq!(saved.model, CODE, "rebuild={rebuild}");
        engine.close();
    }
}
