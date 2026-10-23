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

#[test]
fn relay_config_and_extension_directory_are_owner_private() {
    let dir = tempfile::tempdir().unwrap();
    let folder = install(dir.path(), 9300).unwrap();
    let config = folder.join("relay-config.js");
    for path in [&folder, &config] {
        let file = if path.is_dir() {
            crate::platform::fs::open_dir_nofollow(path).unwrap()
        } else {
            std::fs::File::open(path).unwrap()
        };
        assert!(crate::platform::fs::is_owner_only(&file).unwrap());
    }
    install(dir.path(), 9301).unwrap();
    assert!(crate::platform::fs::is_owner_only(&std::fs::File::open(config).unwrap()).unwrap());
}

#[cfg(unix)]
#[test]
fn installing_refuses_symlinked_extension_directories_and_configs() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let folder = extension_dir(dir.path());
    symlink(outside.path(), &folder).unwrap();
    assert!(install(dir.path(), 9300).is_err());
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    std::fs::remove_file(&folder).unwrap();
    std::fs::create_dir(&folder).unwrap();
    let target = outside.path().join("unrelated");
    std::fs::write(&target, "unchanged").unwrap();
    symlink(&target, folder.join("relay-config.js")).unwrap();
    assert!(install(dir.path(), 9300).is_err());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "unchanged");
}

#[test]
fn installing_refuses_a_directory_in_place_of_the_config() {
    let dir = tempfile::tempdir().unwrap();
    let config = extension_dir(dir.path()).join("relay-config.js");
    std::fs::create_dir_all(&config).unwrap();
    assert!(install(dir.path(), 9300).is_err());
    assert!(config.is_dir());
}
