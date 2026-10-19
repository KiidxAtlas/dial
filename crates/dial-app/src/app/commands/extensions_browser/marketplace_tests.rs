use super::*;
use crate::tools::integrations::plugin::marketplace::DEFAULT_NAME;

fn config_in(dir: &tempfile::TempDir) -> Config {
    Config {
        user_dir: dir.path().into(),
        project_root: dir.path().into(),
        ..Default::default()
    }
}

fn layout_for(config: &Config) -> Layout {
    Layout::with_user_dir(&config.user_dir, config.user_dir.clone()).expect("layout")
}

#[test]
fn default_config_offers_curated_mcp_and_default_marketplace_skills_offline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_in(&dir);
    let rows = snapshot(&layout_for(&config), &config);
    assert!(rows.mcp.iter().any(|row| row.origin == Origin::Curated));
    assert!(
        rows.marketplaces
            .iter()
            .any(|row| row.id == DEFAULT_NAME && row.state == State::Enabled)
    );
    assert!(
        rows.skills
            .iter()
            .any(|row| row.title == "pdf" && row.origin == Origin::Remote)
    );
    assert!(rows.plugins.is_empty());
}

#[test]
fn installed_skills_hide_the_same_skill_from_the_available_list() {
    let listed = |name: &str, marketplace: &str| {
        (
            marketplace.to_owned(),
            SkillEntry {
                name: name.into(),
                description: String::new(),
                url: format!("https://example.com/{marketplace}.git#{name}"),
            },
        )
    };
    let mut taken = BTreeSet::from(["pdf".to_owned()]);
    let rows = available_skills(
        vec![listed("pdf", "a"), listed("docx", "a"), listed("docx", "b")],
        &mut taken,
    );
    let titles: Vec<&str> = rows.iter().map(|row| row.title.as_str()).collect();
    assert_eq!(titles, vec!["docx"]);
    assert!(rows[0].tags.iter().any(|tag| tag == "a"));
}

#[test]
fn skill_install_rejects_path_traversal_before_fetching() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_in(&dir);
    let progress = SharedProgress::default();
    let result = install_skill(
        &layout_for(&config),
        "https://example.com/org/skills.git#../outside",
        &progress,
    );
    assert!(result.is_err());
    assert!(!dir.path().join("skill-packages").exists());
}

#[test]
fn adding_removing_and_disabling_marketplaces_persist_through_the_intents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_in(&dir);
    let layout = layout_for(&config);
    let index = dir.path().join("extra.json");
    std::fs::write(
        &index,
        r#"{"skills":[{"name":"extra-skill","url":"https://example.com/org/skills.git#extra-skill"}]}"#,
    )
    .expect("write index");
    let endpoints = Endpoints::at("http://127.0.0.1:9");
    let progress = SharedProgress::default();
    let names = |config: &Config| -> Vec<String> {
        Marketplaces::new(marketplaces_root(config))
            .list()
            .expect("list")
            .into_iter()
            .map(|market| market.name)
            .collect()
    };
    let go =
        |intent: Intent| run_at(&endpoints, &layout, &config, &intent, &progress).expect("runs");

    let initial = names(&config);
    assert!(
        go(Intent::AddMarketplace {
            name: "extra".into(),
            index: index.display().to_string(),
        })
        .ok
    );
    let mut expected = initial.clone();
    expected.push("extra".to_string());
    expected.sort();
    assert_eq!(names(&config), expected);

    assert!(
        go(Intent::RemoveMarketplace {
            name: "extra".into()
        })
        .ok
    );
    assert_eq!(names(&config), initial);

    assert!(
        go(Intent::ToggleMarketplace {
            name: DEFAULT_NAME.into(),
            enable: false,
        })
        .ok
    );
    let info = Marketplaces::new(marketplaces_root(&config))
        .list()
        .expect("list");
    assert!(
        !info
            .iter()
            .find(|market| market.name == DEFAULT_NAME)
            .unwrap()
            .enabled
    );
}
