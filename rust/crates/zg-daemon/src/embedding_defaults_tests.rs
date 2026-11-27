use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zg_engine::api::index::{
    IndexOptions,
    embedding::{EmbeddingSelection, EmbeddingSelectionSource},
};

#[tokio::test]
async fn caller_defaults_require_a_capability_probe_using_the_original_v2_request() {
    for supported in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("probe");
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                headers.push(stream.read_u8().await.expect("header"));
            }
            let headers = String::from_utf8(headers).expect("headers");
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("length"))
                })
                .expect("content length");
            let mut request = vec![0; length];
            stream.read_exact(&mut request).await.expect("setup body");
            let request: serde_json::Value =
                serde_json::from_slice(&request).expect("setup request");
            assert_eq!(
                request,
                serde_json::json!({
                    "kind": "index_setup",
                    "request": {"model_cache": null, "preferred_model": "local/bekko-256-int8", "credential_available": false}
                })
            );
            let mut setup =
                serde_json::json!({"models": [], "default_model": "local/bekko-256-int8"});
            if supported {
                setup["supports_embedding_defaults"] = true.into();
            }
            let body = serde_json::json!({"status": "success", "value": {"kind": "index_setup", "reply": setup}}).to_string();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("setup reply");
        });
        let request = IndexOptions {
            embedding_default: Some(EmbeddingSelection {
                model: "local/bekko-256-int8".into(),
                source: EmbeddingSelectionSource::Automatic,
                analysis: None,
            }),
            ..Default::default()
        };
        let result = super::ensure_embedding_default_support(
            &format!("http://{address}/admin/execute"),
            &request,
        )
        .await;
        server.await.expect("probe server");
        if supported {
            result.expect("supported default semantics");
        } else {
            let error = result
                .expect_err("older v2 daemon needs restart")
                .to_string();
            assert!(error.contains("Restart"), "{error}");
            assert!(error.contains("no indexing request was sent"), "{error}");
        }
    }
    super::ensure_embedding_default_support("unused", &IndexOptions::default())
        .await
        .expect("requests without caller defaults require no probe");
}
