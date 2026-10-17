use super::*;
use crate::config::tools::dap::DapTransport;
use crate::config::tools::dap::defaults;
use serde_json::Value;

#[test]
fn bundled_catalogue_has_all_adapters() {
    let all = defaults();
    assert_eq!(all.len(), 14);
    for name in ORDER {
        assert!(all.contains_key(*name), "{name}");
    }
    assert!(matches!(all["dlv"].transport, DapTransport::Socket));
    assert!(matches!(all["codelldb"].transport, DapTransport::Tcp));
    assert_eq!(
        all["debugpy"].launch_defaults["justMyCode"],
        Value::Bool(false)
    );
}

#[cfg(unix)]
#[test]
fn selection_prefers_extension_then_marker_and_requires_installation() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("fake-py-adapter");
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut catalogue = BTreeMap::new();
    let mut py = defaults().remove("debugpy").unwrap();
    py.program = bin.clone();
    catalogue.insert("py".to_owned(), py);
    let mut missing = defaults().remove("gdb").unwrap();
    missing.program = dir.path().join("absent-gdb");
    catalogue.insert("gdb".to_owned(), missing);
    std::fs::write(dir.path().join("main.py"), "").unwrap();
    let (name, _) = select(
        &catalogue,
        None,
        Some(&dir.path().join("main.py")),
        dir.path(),
    )
    .unwrap();
    assert_eq!(name, "py");
    let err = select(
        &catalogue,
        None,
        Some(&dir.path().join("main.c")),
        dir.path(),
    )
    .unwrap_err();
    assert!(err.contains("not installed"), "{err}");
    std::fs::write(dir.path().join("pyproject.toml"), "").unwrap();
    let (name, _) = select(&catalogue, None, None, dir.path()).unwrap();
    assert_eq!(name, "py");
    assert!(select(&catalogue, Some("nope"), None, dir.path()).is_err());
}

#[test]
fn glob_root_markers_match_suffixes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("App.sln"), "").unwrap();
    assert!(root_match(dir.path(), dir.path(), &["*.sln".to_owned()]));
    assert!(!root_match(
        dir.path(),
        dir.path(),
        &["*.csproj".to_owned()]
    ));
}
