use super::*;

#[test]
fn install_writes_the_extension_and_the_relay_config() {
    let dir = tempfile::tempdir().unwrap();
    let folder = install(dir.path(), 9300).unwrap();
    assert_eq!(folder, extension_dir(dir.path()));
    let manifest = std::fs::read_to_string(folder.join("manifest.json")).unwrap();
    assert!(manifest.contains("\"manifest_version\": 3"), "{manifest}");
    let config = std::fs::read_to_string(folder.join("relay-config.js")).unwrap();
    let token = super::super::load_or_create_token(dir.path()).unwrap();
    assert!(config.contains("\"port\":9300"), "{config}");
    assert!(config.contains(&token), "{config}");
}

#[test]
fn reinstalling_keeps_the_same_token() {
    let dir = tempfile::tempdir().unwrap();
    install(dir.path(), 9224).unwrap();
    let first = std::fs::read_to_string(extension_dir(dir.path()).join("relay-config.js")).unwrap();
    install(dir.path(), 9224).unwrap();
    let second =
        std::fs::read_to_string(extension_dir(dir.path()).join("relay-config.js")).unwrap();
    assert_eq!(first, second);
}
