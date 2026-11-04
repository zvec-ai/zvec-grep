use super::*;

#[tokio::test]
async fn definitions_without_text_locations_do_not_suppress_suggestions() {
    let fixture = Fixture::new();
    let store = fixture.open_store();
    for (scope, range) in [
        ("Full", Range::Full),
        (
            "Bytes",
            Range::Byte(crate::domain::ByteRange::new(0, 20).expect("byte range")),
        ),
    ] {
        let mut entities = write_symbols(
            &store,
            &format!("{scope}.rs"),
            &[("unlocated", Some(scope), 1, "fn unlocated() {}")],
        );
        let entity = &mut entities[0];
        let id = EntityId::new(entity.file_id(), entity.content(), range).expect("identity");
        let fragments = vec![EntityFragment {
            id: FragmentId::new(&id, 0),
            range: Range::Full,
        }];
        *entity = Entity::new(
            id,
            entity.file_id(),
            range,
            entity.content().clone(),
            entity.metadata().cloned(),
            fragments,
        )
        .expect("unlocated entity");
        let file = store
            .list_files()
            .expect("files")
            .into_iter()
            .find(|file| file.id() == entity.file_id())
            .expect("owner");
        let fragment = IndexedFragment {
            entity_id: entity.id().clone(),
            fragment_id: entity.fragments()[0].id.clone(),
            model: schema().model().reference(),
            vector: vec![1.0, 0.0, 0.0],
            fts_text: "unlocated".into(),
        };
        store
            .replace_file(&file, &entities, &[fragment])
            .expect("definition without text coordinates");
    }
    store.close().expect("publish definitions");
    for query in ["unlocated", "Full::unlocated", "Bytes::unlocated"] {
        let result = fixture.callers(query).await;
        assert!(result.matches.is_empty());
        assert!(result.suggestions.is_empty());
    }
    fixture.add_symbols(
        "suggested.rs",
        &[(
            "unlocated_extra",
            None,
            1,
            "fn unlocated_extra() { /* unlocated */ }",
        )],
    );
    for query in [
        "unlocated",
        "Full::unlocated",
        "Bytes::unlocated",
        "Wrong::unlocated",
    ] {
        let result = fixture.callers(query).await;
        assert!(result.matches.is_empty(), "{query}");
        assert_eq!(result.suggestions.len(), 1, "{query}");
        assert_eq!(result.suggestions[0].name, "unlocated_extra", "{query}");
    }

    // A usable exact definition still takes precedence over similarly named suggestions.
    fixture.add_symbols("located.rs", &[("unlocated", None, 1, "fn unlocated() {}")]);
    let result = fixture.callers("unlocated").await;
    assert_eq!(result.matches.len(), 1);
    assert_eq!(
        result.matches[0].symbol.file_path,
        PathBuf::from("located.rs")
    );
    assert!(result.suggestions.is_empty());
}

#[tokio::test]
async fn suggestions_use_definition_names_rank_similarity_and_count_distinct_definitions() {
    let fixture = Fixture::new();
    fixture.add_symbols("other.rs", &[("new", Some("Other"), 1, "fn new() {}")]);
    fixture.add_symbols(
        "foo.rs",
        &[("helper", Some("Foo"), 1, "fn helper() { new(); }")],
    );
    let builders: Vec<_> = (1..=8)
        .map(|line| {
            (
                "newBuilder",
                Some("Foo"),
                line,
                "fn newBuilder() { /* builder */ }",
            )
        })
        .collect();
    fixture.add_symbols("builders.rs", &builders);
    let names: Vec<_> = [
        "get",
        "user",
        "name",
        "ge",
        "helper",
        "wrap_get_user_name",
        "get_user_name_extra",
        "GET_USER_NAME",
    ]
    .into_iter()
    .enumerate()
    .map(|(i, name)| (name, None, i + 1, "get_user_name ge_more"))
    .collect();
    fixture.add_symbols("names.rs", &names);
    // Prefix order is significant; equally ranked remaining candidates may vary.
    for (query, prefix, rest, count) in [
        ("needle", &[][..], &[][..], 0),
        ("red::needle", &[][..], &[][..], 0),
        ("Foo::new", &["Other::new"][..], &[][..], 1),
        ("absentneedle", &[][..], &[][..], 0),
        (
            "RED::RUN",
            &[][..],
            &["red::run", "blue::run", "run"][..],
            3,
        ),
        ("WrongScope::BUILDER", &[][..], &["Foo::newBuilder"][..], 5),
        (
            "get_user_name",
            &["GET_USER_NAME", "get_user_name_extra", "wrap_get_user_name"][..],
            &["get", "user", "name"][..],
            5,
        ),
        ("ge_more", &[][..], &[][..], 0),
    ] {
        let result = fixture.callers_with_limit(query, 1).await;
        assert!(result.matches.is_empty(), "{query}");
        let names: Vec<_> = result
            .suggestions
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert_eq!(names.len(), count, "{query}");
        assert!(names.starts_with(prefix), "{query}: {names:?}");
        assert!(
            names[prefix.len()..].iter().all(|name| rest.contains(name)),
            "{query}: {names:?}"
        );
        // Every definition has two fragments: five results must still be five definitions.
        let unique: std::collections::HashSet<_> = result
            .suggestions
            .iter()
            .map(|s| (&s.file_path, s.start_line, &s.name))
            .collect();
        assert_eq!(unique.len(), count, "{query}");
        let wire = serde_json::to_value(&result).expect("wire");
        assert!(
            wire["suggestions"]
                .as_array()
                .expect("suggestions")
                .iter()
                .all(|s| s.get("edges").is_none() && s.get("truncated").is_none())
        );
        if let Some(candidate) = result.suggestions.first() {
            let retried = fixture.callers(&candidate.name).await;
            assert!(!retried.matches.is_empty());
            assert!(retried.suggestions.is_empty());
        }
    }
}

#[tokio::test]
async fn wrong_scope_suggests_exact_names_even_when_fts_is_full_of_callers() {
    let fixture = Fixture::new();
    let store = fixture.open_store();
    let callers: Vec<_> = (1..=120)
        .map(|line| ("caller", Some("Calls"), line, "fn caller() { frob(); }"))
        .collect();
    write_symbols(&store, "callers.rs", &callers);
    let body = format!("fn frob() {{ /* {} */ }}", "padding ".repeat(1000));
    let target = write_symbols(&store, "bar.rs", &[("frob", Some("Bar"), 1, &body)]);
    let hits = store.search_fts("frob", 100, None).expect("FTS candidates");
    assert_eq!(hits.len(), 100);
    assert!(
        hits.iter().all(|hit| &hit.entity_id != target[0].id()),
        "fixture reproduces FTS starvation"
    );
    store.close().expect("publish fixture");
    let result = fixture.callers("Foo::frob").await;
    assert!(result.matches.is_empty());
    assert_eq!(result.suggestions.len(), 1);
    assert_eq!(result.suggestions[0].name, "Bar::frob");
}

#[tokio::test]
async fn malformed_fts_and_unreadable_suggestions_do_not_fail_exact_misses() {
    let fixture = Fixture::new();
    let home = fixture.storage_home();
    let candidate = fixture.add_symbols("frob.rs", &[("frob", None, 1, "fn frob() {}")]);
    corrupt_entity_bodies(home, &[candidate[0].id().clone()]);
    let engine = &fixture.engine;
    for symbol in ["wrong::frob", "FROB", "operator()", "\"", "frob AND ("] {
        let result = fixture.callers(symbol).await;
        assert!(result.matches.is_empty());
        assert!(result.suggestions.is_empty());
    }
    assert!(
        engine.callers(fixture.options("frob")).await.is_err(),
        "exact-match corruption remains an error"
    );
}
