use std::{collections::BTreeSet, path::PathBuf};

use zg_testkit::{FixtureError, load_server_option_cases};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../compat/cli/server-option-precedence.json")
}

#[test]
fn real_server_option_fixture_loads() {
    let fixture = load_server_option_cases(&fixture_path()).expect("server option fixture");
    assert_eq!(fixture.cases.len(), 56);
    let ids: BTreeSet<_> = fixture.cases.iter().map(|case| &case.id).collect();
    assert_eq!(ids.len(), fixture.cases.len());
}

#[test]
fn invalid_difference_field_pairs_report_the_case() {
    let original: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture_path()).expect("fixture bytes"))
            .expect("fixture JSON");
    let directory = tempfile::tempdir().expect("private fixture directory");
    let path = directory.path().join("invalid.json");
    for field in ["allowed_differences", "expected_rust"] {
        let mut fixture = original.clone();
        let case = fixture["cases"][0].as_object_mut().expect("case object");
        let id = case["id"].as_str().expect("case ID").to_owned();
        assert!(case.remove(field).is_some());
        std::fs::write(&path, serde_json::to_vec(&fixture).expect("fixture JSON"))
            .expect("write invalid fixture");
        match load_server_option_cases(&path) {
            Err(FixtureError::InvalidCase {
                path: error_path,
                case_id,
            }) => {
                assert_eq!(error_path, path.display().to_string());
                assert_eq!(case_id, id);
            }
            other => panic!("expected InvalidCase after removing {field}, got {other:?}"),
        }
    }
}
