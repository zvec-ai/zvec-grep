use super::*;

#[tokio::test]
async fn exact_definitions_keep_scopes_order_and_per_symbol_limits() {
    let fixture = Fixture::new();
    let engine = &fixture.engine;
    let groups = fixture.callers(" run ").await.matches;
    let descriptions: Vec<_> = groups
        .iter()
        .map(|g| {
            (
                g.symbol.name.as_str(),
                g.symbol.file_path.to_str().expect("fixture path"),
                g.symbol.start_line,
                g.symbol.end_line,
                g.edges.len(),
                g.truncated,
            )
        })
        .collect();
    assert_eq!(
        descriptions,
        [
            ("red::run", "a.rs", 2, 2, 20, true),
            ("blue::run", "a.rs", 10, 10, 20, true),
            ("run", "z.rs", 3, 3, 1, false),
        ],
        "exact names exclude content-only FTS hits and preserve ordering"
    );
    assert_eq!(
        groups[0].edges[0].symbol.as_ref().expect("caller").name,
        "invoke"
    );
    assert_eq!(
        (groups[0].edges[0].line, groups[0].edges[0].column),
        (Some(1), Some(4))
    );
    assert_eq!(
        groups[0].edges[19].line,
        Some(20),
        "keep distinct callsites"
    );

    engine
        .enable_read_session_cache()
        .expect("enable read cache");
    assert_eq!(fixture.callers("run").await.matches, groups,);

    let limited = fixture.callers_with_limit("run", 2).await.matches;
    assert_eq!(
        limited
            .iter()
            .map(|group| group.edges.len())
            .collect::<Vec<_>>(),
        [2, 2, 1]
    );
    assert!(limited[0].truncated);
    let scoped = fixture.callers("blue::run").await.matches;
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].symbol.name, "blue::run");
    assert!(scoped[0].truncated);
    assert_eq!(
        engine.runtime_snapshot(),
        EngineRuntimeSnapshot::default(),
        "relationships do not load embedding models"
    );
}

#[tokio::test]
async fn common_names_are_bounded_after_scope_filtering_without_fts_fallback() {
    let fixture = Fixture::new();
    let common: Vec<_> = (1..=120)
        .map(|line| {
            (
                "new",
                Some(if line <= 100 { "Other" } else { "Rest" }),
                line,
                "fn new() {}",
            )
        })
        .collect();
    fixture.add_symbols("many.rs", &common);
    let foo = fixture.add_symbols(
        "foo.rs",
        &[
            ("helper", Some("Foo"), 5, "fn helper() { new(); }"),
            ("new", Some("Foo"), 10, "fn new() {}"),
        ],
    );
    insert_call(
        &fixture,
        foo[0].file_id().get(),
        foo[0].id().as_str(),
        foo[1].id().as_str(),
    );
    let engine = &fixture.engine;
    let scoped = fixture.callers("Foo::new").await.matches;
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].symbol.name, "Foo::new");
    assert_eq!(scoped[0].edges.len(), 1);
    assert!(!scoped[0].truncated);
    assert_eq!(
        scoped[0].edges[0].symbol.as_ref().expect("caller").name,
        "Foo::helper"
    );
    let others = fixture.callers("Other::new").await.matches;
    assert_eq!(others.len(), 100);
    assert!(others.iter().all(|group| group.symbol.name == "Other::new"));
    let error = engine
        .callees(RelationshipOptions {
            limit: Some(1),
            ..fixture.options("new")
        })
        .await
        .expect_err("too many definitions even with a small edge limit");
    assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
    assert!(
        error
            .message()
            .contains("more than 100 indexed definitions")
    );
    let suggestions = fixture.callers("MissingScope::new").await;
    assert!(suggestions.matches.is_empty());
    assert_eq!(suggestions.suggestions.len(), 5);
    assert!(
        suggestions
            .suggestions
            .iter()
            .all(|symbol| symbol.name.ends_with("::new"))
    );
}

#[tokio::test]
async fn symbol_spellings_select_expected_scopes() {
    let fixture = Fixture::new();
    fixture.add_symbols(
        "scopes.rs",
        &[
            ("main", None, 1, "fn main() {}"),
            ("main", Some("Foo"), 2, "fn main() {}"),
            ("f", Some("ns"), 3, "fn f() {}"),
            ("empty_scope", Some(""), 4, "fn empty_scope() {}"),
            ("bar", Some("ns::Foo"), 5, "fn bar() {}"),
            ("bar", Some("pkg.Foo"), 6, "fn bar() {}"),
        ],
    );
    let engine = &fixture.engine;
    for (query, expected) in [
        ("::main", &["main"][..]),
        ("main", &["main", "Foo::main"][..]),
        ("::ns::f", &["ns::f"][..]),
        ("ns::f", &["ns::f"][..]),
        ("::empty_scope", &["empty_scope"][..]),
        ("red.run", &["red::run"][..]),
        ("red::run", &["red::run"][..]),
        ("ns::Foo::bar", &["ns::Foo::bar"][..]),
        ("ns.Foo.bar", &["ns::Foo::bar"][..]),
        ("ns::Foo.bar", &["ns::Foo::bar"][..]),
        ("::ns.Foo.bar", &["ns::Foo::bar"][..]),
        ("pkg::Foo::bar", &["pkg.Foo::bar"][..]),
        ("pkg.Foo.bar", &["pkg.Foo::bar"][..]),
        ("pkg.Foo::bar", &["pkg.Foo::bar"][..]),
    ] {
        let result = fixture.callers(query).await;
        assert_eq!(
            result
                .matches
                .iter()
                .map(|m| m.symbol.name.as_str())
                .collect::<Vec<_>>(),
            expected,
            "{query}"
        );
        assert!(result.suggestions.is_empty());
    }
    let error = engine
        .callers(fixture.options("::"))
        .await
        .expect_err("empty name");
    assert_eq!(error.message(), "symbol name must not be empty");
}

#[tokio::test]
async fn glob_filters_disambiguate_common_scoped_and_top_level_names_before_the_cap() {
    let fixture = Fixture::new();
    let store = fixture.open_store();
    for index in 0..=100 {
        write_symbols(
            &store,
            &format!("pkg{index}/main.rs"),
            &[
                ("setup", Some("tests"), 1, "fn setup() {}"),
                ("main", None, 2, "fn main() {}"),
            ],
        );
    }
    store.close().expect("close writer");
    let engine = &fixture.engine;
    for name in ["tests::setup", "::main"] {
        let error = engine
            .callers(fixture.options(name))
            .await
            .expect_err("ambiguous across files");
        assert!(error.message().contains("glob filter"));
        for path in ["pkg1/**", "pkg1/main.rs", "/pkg1/**"] {
            let options = RelationshipOptions {
                globs: vec![path.into()],
                ..fixture.options(name)
            };
            let result = engine
                .callers(options.clone())
                .await
                .expect("filtered callers");
            assert_eq!(
                result.matches.len(),
                1,
                "directory must not match pkg10 etc"
            );
            assert_eq!(
                result.matches[0].symbol.file_path,
                PathBuf::from("pkg1/main.rs")
            );
        }
    }
    let suggestions = engine
        .callers(RelationshipOptions {
            globs: vec!["pkg99/**".into()],
            ..fixture.options("wrong::setup")
        })
        .await
        .expect("path-scoped exact-name suggestions");
    assert_eq!(suggestions.suggestions.len(), 1);
    assert_eq!(
        suggestions.suggestions[0].file_path,
        PathBuf::from("pkg99/main.rs")
    );
    for path in ["", "!", "["] {
        let error = engine
            .callers(RelationshipOptions {
                globs: vec![path.into()],
                ..fixture.options("main")
            })
            .await
            .expect_err("invalid glob filter");
        assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
    }
}

#[tokio::test]
async fn glob_filters_exclude_other_files_before_loading_symbol_metadata() {
    let fixture = Fixture::new();
    fixture.add_symbols("keep/main.rs", &[("localize", None, 1, "fn localize() {}")]);
    let outside = fixture.add_symbols(
        "keepers/other.rs",
        &[("localize", None, 1, "fn localize() {}")],
    );
    corrupt_entity_metadata(fixture.storage_home(), &[outside[0].id().clone()]);
    let engine = &fixture.engine;
    assert!(
        engine.callers(fixture.options("localize")).await.is_err(),
        "unfiltered metadata is corrupt"
    );
    for path in ["keep/**", "keep/main.rs", "missing/**"] {
        for (name, exact) in [
            ("localize", true),
            ("wrong::localize", false),
            ("LOCALIZE", false),
        ] {
            let result = engine
                .callers(RelationshipOptions {
                    globs: vec![path.into()],
                    ..fixture.options(name)
                })
                .await
                .expect("outside metadata must not be fetched");
            let count = usize::from(path != "missing/**");
            assert_eq!(
                result.matches.len(),
                if exact { count } else { 0 },
                "{path}: {name}"
            );
            assert_eq!(
                result.suggestions.len(),
                if exact { 0 } else { count },
                "{path}: {name}"
            );
        }
    }
}

#[tokio::test]
async fn ordered_and_insensitive_globs_filter_definitions_but_not_call_sites() {
    use crate::domain::GlobRule;
    let fixture = Fixture::new();
    for globs in [
        vec!["*.rs".into(), "!z.rs".into()],
        vec!["*.rs".into(), "!a.rs".into(), "a.rs".into(), "!z.rs".into()],
        vec![GlobRule {
            pattern: "A.RS".into(),
            case_insensitive: true,
        }],
    ] {
        let result = fixture
            .engine
            .callers(RelationshipOptions {
                globs: globs.clone(),
                ..fixture.options("run")
            })
            .await
            .expect("filtered definitions");
        assert_eq!(result.matches.len(), 2);
        assert!(
            result
                .matches
                .iter()
                .all(|m| m.symbol.file_path == std::path::Path::new("a.rs"))
        );
        let result = fixture
            .engine
            .callees(RelationshipOptions {
                globs,
                limit: Some(100),
                ..fixture.options("invoke")
            })
            .await
            .expect("unrestricted callees");
        assert!(result.matches[0].edges.iter().any(|edge| {
            edge.symbol
                .as_ref()
                .is_some_and(|symbol| symbol.file_path == std::path::Path::new("z.rs"))
        }));
    }
}
