use super::*;
use serde_json::json;

fn fixture(files: &[(&str, Value)]) -> (tempfile::TempDir, RuleCaches) {
    let directory = tempfile::tempdir().expect("configuration workspace");
    for (path, value) in files {
        let path = directory.path().join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, value.to_string()).expect("configuration");
    }
    let caches = RuleCaches {
        source_root: Some(directory.path().into()),
        ..RuleCaches::default()
    };
    (directory, caches)
}

#[test]
fn extends_arrays_override_defined_options_and_replace_paths_wholly() {
    let first = json!({"@/*":["src/*"], "keep/*":["kept/*"]});
    let second = json!({"@/*":["other/*"]});
    let (directory, mut caches) = fixture(&[
        (
            "configs/a.json",
            json!({"compilerOptions":{"baseUrl":"../app", "paths":first}}),
        ),
        (
            "configs/b.json",
            json!({"compilerOptions":{"paths":second}}),
        ),
        (
            "configs/empty.json",
            json!({"compilerOptions":{"paths":{}}}),
        ),
        (
            "configs/options.json",
            json!({"compilerOptions":{"strict":true}}),
        ),
        (
            "configs/nested.json",
            json!({"extends":["./a", "./b.json"]}),
        ),
    ]);
    for (index, (bases, expected)) in [
        (json!(["./configs/a", "./configs/b.json"]), second.clone()),
        (json!(["./configs/b.json", "./configs/a"]), first.clone()),
        (json!(["./configs/a", "./configs/options.json"]), first),
        (json!(["./configs/a", "./configs/empty.json"]), json!({})),
        (json!(["./configs/nested.json"]), second),
    ]
    .into_iter()
    .enumerate()
    {
        let path = format!("child-{index}.json");
        std::fs::write(
            directory.path().join(&path),
            json!({"extends":bases}).to_string(),
        )
        .expect("child config");
        let config = typescript(&mut caches, &path)
            .expect("parse")
            .expect("present");
        assert_eq!(config.base_url.as_deref(), Some("app"), "{bases}");
        assert_eq!(config.paths.as_ref(), expected.as_object(), "{bases}");
        assert_eq!(config.paths_directory, "configs");
    }
}

#[test]
fn independent_child_mappings_do_not_require_readable_parents() {
    let paths = json!({"@/*":["src/*"]});
    for extends in [
        json!("@tsconfig/node20/tsconfig.json"),
        json!(["./missing.json", "@tsconfig/node20/tsconfig.json"]),
        json!("./child.json"),
    ] {
        for (compiler, succeeds) in [
            (json!({"baseUrl":".", "paths":paths}), true),
            (json!({"paths":paths}), false),
            (json!({"baseUrl":"."}), false),
            (json!({"baseUrl":false, "paths":paths}), false),
            (json!({"baseUrl":".", "paths":[]}), false),
        ] {
            let (_directory, mut caches) = fixture(&[(
                "app/child.json",
                json!({"extends":extends, "compilerOptions":compiler}),
            )]);
            let config = typescript(&mut caches, "app/child.json");
            assert_eq!(config.is_ok(), succeeds, "{extends}: {compiler}");
            if succeeds {
                let config = config.expect("independent mappings").expect("present");
                assert_eq!(config.base_url.as_deref(), Some("app"));
                assert_eq!(config.paths.as_ref(), paths.as_object());
            }
        }
    }
}

#[test]
fn paths_without_base_url_keep_the_declaring_directory() {
    let paths = json!({"@/*":["../src/*"]});
    let (_directory, mut caches) = fixture(&[
        (
            "configs/base.json",
            json!({"compilerOptions":{"paths":paths}}),
        ),
        (
            "app/child.json",
            json!({"extends":["../configs/base.json"]}),
        ),
    ]);
    let config = typescript(&mut caches, "app/child.json")
        .expect("parse")
        .expect("present");
    assert!(config.base_url.is_none());
    assert_eq!(config.paths.as_ref(), paths.as_object());
    assert_eq!(config.paths_directory, "configs");
}
