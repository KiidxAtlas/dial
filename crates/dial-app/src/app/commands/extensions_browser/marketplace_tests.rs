use super::*;

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
fn default_config_offers_curated_mcp_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_in(&dir);
    let rows = snapshot(&layout_for(&config), &config);
    assert!(rows.mcp.iter().any(|row| row.origin == Origin::Curated));
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
