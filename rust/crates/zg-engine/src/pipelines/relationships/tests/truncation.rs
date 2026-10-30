use super::*;

#[tokio::test]
async fn callees_keep_repeated_sites_and_cross_file_edges_but_omit_missing_endpoints() {
    let fixture = Fixture::new();
    let groups = fixture.callees("invoke", 100).await.matches;
    assert_eq!(groups.len(), 1);
    let group = &groups[0];
    assert!(!group.truncated);
    assert_eq!(group.edges.len(), 49);
    assert_eq!(
        group
            .edges
            .iter()
            .filter(|edge| edge
                .symbol
                .as_ref()
                .is_some_and(|symbol| symbol.name == "red::run"))
            .count(),
        25
    );
    assert_eq!(
        group
            .edges
            .iter()
            .filter(|edge| edge
                .symbol
                .as_ref()
                .is_some_and(|symbol| symbol.name == "blue::run"))
            .count(),
        23
    );
    assert!(group.edges.iter().all(|edge| edge.line != Some(99)));
    let remote = group
        .edges
        .iter()
        .find(|edge| edge.line == Some(101))
        .expect("resolved cross-file edge");
    assert_eq!(remote.column, Some(8));
    assert_eq!(
        remote.file_path,
        PathBuf::from("a.rs"),
        "outgoing call position belongs to the caller, not the remote callee"
    );
    assert_eq!(
        remote
            .symbol
            .as_ref()
            .expect("remote description")
            .file_path,
        PathBuf::from("z.rs")
    );
    assert!(group.edges.iter().all(|edge| edge.line != Some(102)));
}

#[tokio::test]
async fn unfinished_cross_file_endpoints_are_filtered_before_limit() {
    for deleting in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.open_store();
        let remote = store
            .list_files()
            .expect("files")
            .into_iter()
            .find(|file| file.relative_path().to_path_buf() == std::path::Path::new("z.rs"))
            .expect("fixture value");
        store
            .set_test_file_status(
                remote.id(),
                if deleting {
                    FileIndexStatus::Deleting
                } else {
                    FileIndexStatus::NotIndexed
                },
            )
            .expect("unfinished canonical file");
        store.close().expect("persist unfinished state");
        let groups = fixture.callees("invoke", 100).await.matches;
        assert!(!groups[0].truncated);
        assert_eq!(groups[0].edges.len(), 48);
        assert!(
            groups[0]
                .edges
                .iter()
                .all(|edge| edge.line != Some(101) && edge.line != Some(99))
        );
        let groups = ZvecGrep::new()
            .callers(fixture.options("run"))
            .await
            .expect("query visible definitions")
            .matches;
        assert_eq!(
            groups.len(),
            2,
            "unfinished definition must not become a query root"
        );
    }
}

#[tokio::test]
async fn truncation_counts_only_visible_edges_and_handles_exact_limits() {
    let fixture = Fixture::new();
    for (limit, count, truncated) in [
        (1, 1, true),
        (48, 48, true),
        (49, 49, false),
        (50, 49, false),
    ] {
        let groups = fixture.callees("invoke", limit).await.matches;
        assert_eq!(
            (groups[0].edges.len(), groups[0].truncated),
            (count, truncated),
            "limit={limit}"
        );
        if count == 49 {
            assert!(groups[0].edges.iter().any(|edge| edge.line == Some(101)));
        }
    }
}

#[tokio::test]
async fn truncation_skips_invisible_edges_in_both_directions() {
    let fixture = Fixture::new();
    let entities = fixture.add_symbols(
        "visibility.rs",
        &[
            ("visible_caller", None, 1, "fn visible_caller() {}"),
            ("visible_target", None, 2, "fn visible_target() {}"),
        ],
    );
    let graph = rusqlite::Connection::open(&fixture.graph_path).expect("graph fixture");
    // Invisible edges must not consume the limit before later visible calls.
    for (source, target) in [
        (entities[0].id().as_str(), "missing"),
        ("missing", entities[1].id().as_str()),
    ] {
        graph
            .execute(
                "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 300)
             INSERT INTO edges (file_id, source, target, kind, status, provenance, line)
             SELECT ?1, ?2, ?3, 'calls', 'resolved', 'file_local', x FROM n",
                rusqlite::params![entities[0].file_id().get(), source, target],
            )
            .expect("invisible edges");
    }
    for _ in 0..3 {
        insert_call(
            &fixture,
            entities[0].file_id().get(),
            entities[0].id().as_str(),
            entities[1].id().as_str(),
        );
    }
    drop(graph);
    let engine = &fixture.engine;
    for (limit, count, truncated) in [(1, 1, true), (3, 3, false), (4, 3, false)] {
        for incoming in [false, true] {
            let options = RelationshipOptions {
                limit: Some(limit),
                ..fixture.options(if incoming {
                    "visible_target"
                } else {
                    "visible_caller"
                })
            };
            let reply = if incoming {
                engine.callers(options).await
            } else {
                engine.callees(options).await
            }
            .expect("advance across invisible edges");
            assert_eq!(
                (reply.matches[0].edges.len(), reply.matches[0].truncated),
                (count, truncated)
            );
            assert!(
                reply.matches[0]
                    .edges
                    .iter()
                    .all(|edge| edge.line == Some(7))
            );
        }
    }
}

#[tokio::test]
async fn limited_queries_skip_unneeded_endpoint_metadata_and_bodies() {
    for corruption in ["endpoint metadata", "caller body"] {
        let fixture = Fixture::new();
        let (direction, symbol) = if corruption == "endpoint metadata" {
            let extra = fixture.add_symbols("extra.rs", &[("extra", None, 1, "fn extra() {}")]);
            let caller = fixture.find_symbol("invoke", None);
            insert_call(
                &fixture,
                caller.file_id().get(),
                caller.id().as_str(),
                extra[0].id().as_str(),
            );
            corrupt_entity_identity(fixture.storage_home(), std::slice::from_ref(extra[0].id()));
            (RelationshipDirection::Callees, "invoke")
        } else {
            let target = fixture.find_symbol("run", Some("red"));
            let caller = fixture.find_symbol("helper", Some("red"));
            insert_call(
                &fixture,
                caller.file_id().get(),
                caller.id().as_str(),
                target.id().as_str(),
            );
            corrupt_entity_bodies(fixture.storage_home(), std::slice::from_ref(caller.id()));
            (RelationshipDirection::Callers, "red::run")
        };
        for limit in [1, 1000] {
            let result = fixture
                .engine
                .relationships(
                    direction,
                    RelationshipOptions {
                        limit: Some(limit),
                        ..fixture.options(symbol)
                    },
                )
                .await;
            if limit == 1 {
                let groups = result.expect(corruption).matches;
                assert_eq!(groups[0].edges.len(), 1, "{corruption}");
                assert!(groups[0].truncated, "{corruption}");
                assert_eq!(groups[0].edges[0].file_path, PathBuf::from("a.rs"));
            } else {
                assert!(result.is_err(), "reading {corruption} must still fail");
            }
        }
    }
}

#[tokio::test]
async fn file_level_callers_keep_their_paths_without_symbol_metadata() {
    let fixture = Fixture::new();
    let store = fixture.open_store();
    let target = store
        .read_snapshot(|view| {
            crate::pipelines::relationships::pipeline::find_symbols(view, "run", Some("red"), None)
        })
        .expect("target")
        .remove(0);
    let sources: Vec<_> = store
        .list_files()
        .expect("files")
        .into_iter()
        .filter(|file| matches!(file.relative_path().to_str(), Some("c.rs" | "z.rs")))
        .collect();
    assert_eq!(sources.len(), 2);
    store.close().expect("close writer");
    for source in sources {
        insert_call(
            &fixture,
            source.id().get(),
            &format!("f{}", source.id()),
            target.entity.id().as_str(),
        );
    }
    let groups = fixture.callers_with_limit("red::run", 100).await.matches;
    assert_eq!(groups[0].edges.len(), 27);
    assert!(!groups[0].truncated);
    let mut paths: Vec<_> = groups[0]
        .edges
        .iter()
        .filter(|edge| edge.symbol.is_none())
        .map(|edge| {
            assert_eq!((edge.line, edge.column), (Some(7), Some(4)));
            edge.file_path.clone()
        })
        .collect();
    paths.sort();
    assert_eq!(paths, [PathBuf::from("c.rs"), PathBuf::from("z.rs")]);
}
