use super::*;
use serde_json::json;

#[test]
fn profile_names_are_a_closed_alphabet() {
    assert!(validate_profile_name("work_1-b").is_ok());
    assert!(validate_profile_name("").is_err());
    assert!(validate_profile_name("../escape").is_err());
    assert!(validate_profile_name("a/b").is_err());
    assert!(validate_profile_name(&"x".repeat(65)).is_err());
}

#[test]
fn profile_paths_stay_under_state_profiles_directory() {
    let path = profile_path(Path::new("/state"), "work").unwrap();
    assert_eq!(path, Path::new("/state/browser/profiles/work.json"));
    assert!(profile_path(Path::new("/state"), "../x").is_err());
}

#[test]
fn save_then_load_round_trips_and_missing_profile_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = profile_path(dir.path(), "default").unwrap();
    assert_eq!(load(&path).unwrap(), None);
    let state = json!({"cookies": [{"name": "sid", "value": "1"}], "origins": []});
    save(&path, &state).unwrap();
    assert_eq!(load(&path).unwrap(), Some(state));
}

#[test]
fn corrupt_profile_is_reported_not_silently_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.json");
    std::fs::write(&path, b"{not json").unwrap();
    assert!(load(&path).is_err());
}
