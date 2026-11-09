use super::*;
use std::fs;

#[test]
fn kernel_integration_is_renamed_to_eval_without_duplicating_it() {
    let mut table: toml::Table = toml::from_str("integrations=['kernel','bg']").unwrap();
    assert!(migrate_merged_integrations(&mut table));
    assert_eq!(
        table,
        toml::from_str::<toml::Table>("integrations=['eval','bg']").unwrap()
    );
    assert!(!migrate_merged_integrations(&mut table));

    let mut both: toml::Table = toml::from_str("integrations=['eval','kernel']").unwrap();
    assert!(migrate_merged_integrations(&mut both));
    assert_eq!(
        both,
        toml::from_str::<toml::Table>("integrations=['eval']").unwrap()
    );
}

#[test]
fn merged_evidence_integrations_collapse_into_one_evidence_entry() {
    let mut table: toml::Table = toml::from_str(
        "integrations=['bg','evidence_search','workbench','evidence','timeline','reality','investigate']",
    )
    .unwrap();
    assert!(migrate_merged_integrations(&mut table));
    assert_eq!(
        table,
        toml::from_str::<toml::Table>("integrations=['bg','evidence']").unwrap()
    );

    let mut without: toml::Table =
        toml::from_str("integrations=['investigate','bg','workbench']").unwrap();
    assert!(migrate_merged_integrations(&mut without));
    assert_eq!(
        without,
        toml::from_str::<toml::Table>("integrations=['evidence','bg']").unwrap()
    );
}

#[test]
fn saved_evidence_family_integrations_still_validate() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let path = layout.config_files()[0].clone();
    fs::write(
        &path,
        "model='main-model'\nprovider='mlx'\nintegrations=['timeline','bg','workbench']\n",
    )
    .unwrap();
    let config = Config::load(&layout, None).unwrap();
    config.validate().unwrap();
    assert_eq!(config.integrations, vec!["evidence", "bg"]);
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
