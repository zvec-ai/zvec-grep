use super::*;
use serde_json::json;

fn write(path: &Path, languages: &Value) {
    fs::write(
        path,
        serde_json::to_vec(&json!({"version": 1, "languages": languages})).expect("JSON"),
    )
    .expect("language file");
}

#[test]
fn fts_languages_builtin_presets_expand_without_creating_a_file() {
    let home = tempfile::tempdir().expect("home");
    let path = home.path().join("fts-languages.json");
    let presets = load_at(&path).expect("built-ins");
    for name in [
        "en", "nl", "ru", "zh", "de", "fr", "es", "pt", "ar", "hi", "it", "tr", "pl", "id",
    ] {
        assert!(presets.contains_key(name), "{name}");
    }
    for preset in presets.values() {
        preset.config.validate().expect("supported preset");
        if preset.name != "zh" {
            assert_eq!(preset.config.tokenizer, "standard");
            assert_eq!(preset.config.filters, ["lowercase", "stemmer"]);
            assert!(preset.config.extra_params["stemmer_lang"].is_string());
        }
    }
    assert_eq!(presets["zh"].config, FtsConfig::default());
    for (name, language) in [("en", "english"), ("nl", "dutch"), ("ru", "russian")] {
        let config = &presets[name].config;
        config.validate().expect("valid preset");
        assert_eq!(config.tokenizer, "standard");
        assert_eq!(config.filters, ["lowercase", "stemmer"]);
        assert_eq!(config.extra_params["stemmer_lang"], language);
        assert!(presets[name].source.is_none());
    }
    let update = resolve_at(&path, " ZH ", &FtsConfigUpdate::default()).expect("normalized name");
    assert_eq!(
        update
            .apply(&presets["ru"].config)
            .expect("replace Russian analysis"),
        FtsConfig::default()
    );
    assert!(!path.exists());
}

#[test]
fn fts_languages_user_definitions_replace_append_and_reload() {
    let home = tempfile::tempdir().expect("home");
    let path = home.path().join("fts-languages.json");
    let custom = json!({"tokenizer": "whitespace", "filters": [], "extraParams": {}});
    write(&path, &json!({"en": custom, "ru-docs": custom}));
    let presets = load_at(&path).expect("user definitions");
    assert_eq!(presets.len(), builtins().len() + 1);
    assert_eq!(presets["en"].source.as_deref(), Some(path.as_path()));
    assert_eq!(
        serde_json::to_value(&presets["en"].config).expect("config"),
        custom
    );
    assert_eq!(presets["ru-docs"].config, presets["en"].config);
    assert!(presets["ru"].source.is_none());
    let saved = resolve_at(&path, "ru-docs", &FtsConfigUpdate::default()).expect("expand");
    let replacement = json!({"tokenizer": "standard", "filters": ["lowercase"], "extraParams": {}});
    write(&path, &json!({"ru-docs": replacement}));
    let next = resolve_at(&path, "ru-docs", &FtsConfigUpdate::default()).expect("reload");
    assert_eq!(saved.tokenizer.as_deref(), Some("whitespace"));
    assert_eq!(next.tokenizer.as_deref(), Some("standard"));
    assert_eq!(
        serde_json::to_value(&next).expect("concrete update"),
        replacement
    );
    let error = resolve_at(&path, "unknown", &FtsConfigUpdate::default())
        .expect_err("unknown name")
        .to_string();
    assert!(error.contains("unknown FTS language"), "{error}");
    assert!(error.contains("ru-docs"), "{error}");
    fs::remove_file(&path).expect("remove overrides");
    assert_eq!(
        load_at(&path).expect("restore built-ins")["en"]
            .config
            .filters,
        ["lowercase", "stemmer"]
    );
}

#[test]
fn fts_languages_explicit_fields_replace_preset_fields_including_empty_values() {
    let home = tempfile::tempdir().expect("home");
    let path = home.path().join("fts-languages.json");
    for overrides in [
        json!({"filters": [], "extraParams": {}}),
        json!({"extraParams": {"stemmer_lang": "english"}}),
        json!({"tokenizer": "jieba", "filters": ["lowercase"], "extraParams": {}}),
    ] {
        let update: FtsConfigUpdate = serde_json::from_value(overrides.clone()).expect("overrides");
        let resolved = resolve_at(&path, "ru", &update).expect("explicit fields");
        let encoded = serde_json::to_value(&resolved).expect("JSON");
        for (key, value) in overrides.as_object().expect("fields") {
            assert_eq!(&encoded[key], value, "{key}");
        }
        assert!(encoded.get("lang").is_none());
    }
    let override_filters = FtsConfigUpdate {
        filters: Some(vec![]),
        ..FtsConfigUpdate::default()
    };
    let resolved = resolve_at(&path, "ru", &override_filters).expect("preserve native options");
    assert_eq!(
        resolved.extra_params.expect("parameters")["stemmer_lang"],
        "russian"
    );
    let omitted = FtsConfigUpdate {
        tokenizer: Some("whitespace".into()),
        ..FtsConfigUpdate::default()
    };
    assert_eq!(resolve(None, omitted.clone()).expect("no preset"), omitted);
}

#[test]
fn fts_languages_invalid_files_do_not_fall_back_to_builtins() {
    let home = tempfile::tempdir().expect("home");
    let path = home.path().join("fts-languages.json");
    for content in [
        "invalid JSON",
        r#"{"version":2,"languages":{}}"#,
        r#"{"version":1,"languages":{},"unknown":true}"#,
        r#"{"version":1,"languages":{"en":{"tokenizer":"standard","filters":[]}}}"#,
        r#"{"version":1,"languages":{"en":{"tokenizer":"","filters":[],"extraParams":{}}}}"#,
        r#"{"version":1,"languages":{"en":{"tokenizer":"standard","filters":[""],"extraParams":{}}}}"#,
        r#"{"version":1,"languages":{"RU":{"tokenizer":"standard","filters":[],"extraParams":{}}}}"#,
        r#"{"version":1,"languages":{"":{"tokenizer":"standard","filters":[],"extraParams":{}}}}"#,
        r#"{"version":1,"languages":{"en":{"tokenizer":"standard","filters":[],"extraParams":{}},"en":{"tokenizer":"jieba","filters":[],"extraParams":{}}}}"#,
    ] {
        fs::write(&path, content).expect("invalid file");
        let error = resolve_at(&path, "en", &FtsConfigUpdate::default())
            .expect_err("invalid mappings")
            .to_string();
        assert!(error.contains("fts-languages.json"), "{error}");
    }
    let error = load_at(home.path())
        .expect_err("directory cannot be a mapping file")
        .to_string();
    assert!(error.contains("read FTS language mappings"), "{error}");
}
