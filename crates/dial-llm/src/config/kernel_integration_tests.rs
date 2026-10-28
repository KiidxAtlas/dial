use super::*;
use std::fs;

#[test]
fn kernel_integration_is_renamed_to_eval_without_duplicating_it() {
    let mut table: toml::Table = toml::from_str("integrations=['kernel','bg']").unwrap();
    assert!(migrate_kernel_integration(&mut table));
    assert_eq!(
        table,
        toml::from_str::<toml::Table>("integrations=['eval','bg']").unwrap()
    );
    assert!(!migrate_kernel_integration(&mut table));

    let mut both: toml::Table = toml::from_str("integrations=['eval','kernel']").unwrap();
    assert!(migrate_kernel_integration(&mut both));
    assert_eq!(
        both,
        toml::from_str::<toml::Table>("integrations=['eval']").unwrap()
    );
}

#[test]
fn saved_kernel_integration_loads_as_eval_and_is_rewritten() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let path = layout.config_files()[0].clone();
    fs::write(
        &path,
        "model='main-model'\nprovider='mlx'\nintegrations=['kernel','bg']\n",
    )
    .unwrap();
    let config = Config::load(&layout, None).unwrap();
    config.validate().unwrap();
    assert!(config.integrations.iter().any(|name| name == "eval"));
    assert!(!config.integrations.iter().any(|name| name == "kernel"));
    let persisted: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        persisted["integrations"],
        toml::Value::try_from(vec!["eval", "bg"]).unwrap()
    );
}
