use super::*;

#[test]
fn pinned_version_must_match_exactly() {
    assert!(check_version("v24.22.3", Some("24.22.3")).is_ok());
    for actual in ["v24.22.2", "v26.0.0", "", "v24.22.3\nextra"] {
        assert!(
            check_version(actual, Some("24.22.3")).is_err(),
            "{actual:?}"
        );
    }
}

#[test]
fn source_runtime_still_requires_supported_node() {
    assert!(check_version("v22.23.3", None).is_ok());
    assert!(check_version("v24.22.3", None).is_ok());
    for actual in ["v20.0.0", "22.0.0", "", "not Node"] {
        assert!(check_version(actual, None).is_err(), "{actual:?}");
    }
}

#[test]
fn unknown_probe_arguments_fail_before_launching_anything() {
    assert!(run([std::ffi::OsString::from("--not-a-probe-option")].into_iter()).is_err());
}
