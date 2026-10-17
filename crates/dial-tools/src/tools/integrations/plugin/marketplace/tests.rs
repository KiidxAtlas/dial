use super::super::installer::test_support::write_plugin;
use super::*;
use serde_json::json;

struct Fixture {
    _temp: tempfile::TempDir,
    base: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().to_path_buf();
        Fixture { _temp: temp, base }
    }

    fn plugin_dir(&self, name: &str, version: &str) -> PathBuf {
        let dir = self.base.join("plugins").join(name);
        write_plugin(&dir, &json!({"name": name, "version": version}).to_string());
        dir
    }

    fn write_index(&self, file: &str, plugins: serde_json::Value) -> PathBuf {
        let path = self.base.join(file);
        fs::write(&path, json!({ "plugins": plugins }).to_string()).unwrap();
        path
    }

    fn store(&self) -> PluginStore {
        PluginStore::new(self.base.join("store"))
    }

    fn marketplaces(&self) -> Marketplaces {
        Marketplaces::new(self.base.join("marketplaces"))
    }
}

#[test]
fn add_list_entries_and_install_by_reference() {
    let fx = Fixture::new();
    let alpha = fx.plugin_dir("alpha", "1.0.0");
    let index = fx.write_index(
        "index.json",
        json!([{
            "name": "alpha",
            "version": "1.0.0",
            "source": format!("file:{}", alpha.display()),
            "description": "first"
        }]),
    );
    let marketplaces = fx.marketplaces();
    marketplaces.add("local", index.to_str().unwrap()).unwrap();

    let infos = marketplaces.list().unwrap();
    assert_eq!(infos.len(), BUILTINS.len() + 1);
    let local = infos.iter().find(|info| info.name == "local").unwrap();
    assert!(local.refreshed_at.is_some());

    let entries = marketplaces.entries(None).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].marketplace, "local");
    assert_eq!(entries[0].name, "alpha");
    assert_eq!(entries[0].description.as_deref(), Some("first"));

    let store = fx.store();
    let installed = marketplaces.install("alpha@local", &store).unwrap();
    assert_eq!(installed.name, "alpha");
    assert_eq!(installed.version.as_deref(), Some("1.0.0"));
    assert_eq!(store.list().unwrap().len(), 1);

    let err = marketplaces
        .add("local", index.to_str().unwrap())
        .unwrap_err();
    assert!(format!("{err:#}").contains("already exists"));
}

#[test]
fn bare_name_is_ambiguous_across_marketplaces() {
    let fx = Fixture::new();
    let beta_a = fx.base.join("plugins/beta-a");
    write_plugin(
        &beta_a,
        &json!({"name": "beta", "version": "1.0.0"}).to_string(),
    );
    let beta_b = fx.base.join("plugins/beta-b");
    write_plugin(
        &beta_b,
        &json!({"name": "beta", "version": "2.0.0"}).to_string(),
    );
    let index_a = fx.write_index(
        "a.json",
        json!([{"name": "beta", "version": "1.0.0", "source": format!("file:{}", beta_a.display())}]),
    );
    let index_b = fx.write_index(
        "b.json",
        json!([{"name": "beta", "version": "2.0.0", "source": format!("file:{}", beta_b.display())}]),
    );
    let marketplaces = fx.marketplaces();
    marketplaces
        .add("mkt-a", index_a.to_str().unwrap())
        .unwrap();
    marketplaces
        .add("mkt-b", index_b.to_str().unwrap())
        .unwrap();

    let store = fx.store();
    let err = marketplaces.install("beta", &store).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("beta@mkt-a"), "{message}");
    assert!(message.contains("beta@mkt-b"), "{message}");
    assert!(store.list().unwrap().is_empty());

    let installed = marketplaces.install("beta@mkt-b", &store).unwrap();
    assert_eq!(installed.version.as_deref(), Some("2.0.0"));
}

#[test]
fn refresh_and_upgrade_apply_new_version() {
    let fx = Fixture::new();
    let alpha = fx.plugin_dir("alpha", "1.0.0");
    let index = fx.write_index(
        "index.json",
        json!([{"name": "alpha", "version": "1.0.0", "source": format!("file:{}", alpha.display())}]),
    );
    let marketplaces = fx.marketplaces();
    marketplaces.add("local", index.to_str().unwrap()).unwrap();
    let store = fx.store();
    marketplaces.install("alpha", &store).unwrap();

    write_plugin(
        &alpha,
        &json!({"name": "alpha", "version": "2.0.0"}).to_string(),
    );
    fx.write_index(
        "index.json",
        json!([{"name": "alpha", "version": "2.0.0", "source": format!("file:{}", alpha.display())}]),
    );

    let updates = marketplaces.upgrade(&store, None).unwrap();
    assert_eq!(updates.len(), 1);
    assert!(updates[0].changed);
    assert_eq!(
        store.get("alpha").unwrap().version.as_deref(),
        Some("2.0.0")
    );
}

#[test]
fn upgrade_never_substitutes_another_publisher() {
    let fx = Fixture::new();
    let publisher_a = fx.plugin_dir("alpha", "1.0.0");
    let publisher_b = fx.base.join("plugins/alpha-b");
    write_plugin(
        &publisher_b,
        &json!({"name": "alpha", "version": "2.0.0"}).to_string(),
    );
    let source_a = format!("file:{}", publisher_a.display());
    let source_b = format!("file:{}", publisher_b.display());
    let index_a = fx.write_index(
        "a.json",
        json!([{"name": "alpha", "version": "1.0.0", "source": source_a}]),
    );
    let index_b = fx.write_index(
        "b.json",
        json!([{"name": "alpha", "version": "2.0.0", "source": source_b}]),
    );
    let marketplaces = fx.marketplaces();
    marketplaces
        .add("mkt-a", index_a.to_str().unwrap())
        .unwrap();
    marketplaces
        .add("mkt-b", index_b.to_str().unwrap())
        .unwrap();
    let store = fx.store();
    marketplaces.install("alpha@mkt-a", &store).unwrap();

    // Publisher A's catalog is removed; publisher B's same-name entry must not take over.
    marketplaces.remove("mkt-a").unwrap();
    let updates = marketplaces.upgrade(&store, None).unwrap();
    assert!(updates.is_empty(), "{updates:?}");
    let kept = store.get("alpha").unwrap();
    assert_eq!(kept.source, source_a);
    assert_eq!(kept.version.as_deref(), Some("1.0.0"));
    assert!(kept.enabled);
    let err = marketplaces.upgrade(&store, Some("alpha")).unwrap_err();
    assert!(format!("{err:#}").contains("installed source"), "{err:#}");
    assert_eq!(store.get("alpha").unwrap().source, source_a);
}

#[test]
fn remove_drops_registration_and_snapshot() {
    let fx = Fixture::new();
    let alpha = fx.plugin_dir("alpha", "1.0.0");
    let index = fx.write_index(
        "index.json",
        json!([{"name": "alpha", "source": format!("file:{}", alpha.display())}]),
    );
    let marketplaces = fx.marketplaces();
    marketplaces.add("local", index.to_str().unwrap()).unwrap();
    marketplaces.remove("local").unwrap();
    assert_eq!(marketplaces.list().unwrap().len(), BUILTINS.len());
    assert!(marketplaces.entries(None).unwrap().is_empty());
    assert!(!fx.base.join("marketplaces/local.json").exists());
    assert!(marketplaces.remove("local").is_err());
}

#[test]
fn reserved_and_invalid_names_are_rejected() {
    let fx = Fixture::new();
    let marketplaces = fx.marketplaces();
    assert!(marketplaces.add("marketplaces", "missing.json").is_err());
    assert!(marketplaces.add("../escape", "missing.json").is_err());
}

fn skill_index(fx: &Fixture, file: &str, skills: serde_json::Value) -> PathBuf {
    let path = fx.base.join(file);
    fs::write(&path, json!({ "skills": skills }).to_string()).unwrap();
    path
}

#[test]
fn builtin_marketplaces_are_present_offline_and_removable() {
    let fx = Fixture::new();
    let marketplaces = fx.marketplaces();
    let infos = marketplaces.list().unwrap();
    assert_eq!(infos.len(), BUILTINS.len());
    assert!(infos.iter().all(|info| info.builtin && info.enabled));
    // Browsing creates neither registry/snapshot files nor installed packages.
    let skills = marketplaces.skills(None).unwrap();
    for (marketplace, skill) in [
        (DEFAULT_NAME, "pdf"),
        ("openai-skills", "gh-fix-ci"),
        ("vercel-skills", "web-design-guidelines"),
    ] {
        assert!(
            skills
                .iter()
                .any(|s| s.name == skill && s.marketplace == marketplace)
        );
    }
    assert!(marketplaces.entries(None).unwrap().is_empty());
    marketplaces.refresh(None).unwrap();
    assert_eq!(fs::read_dir(&fx.base).unwrap().count(), 0);

    for &(name, _) in BUILTINS {
        marketplaces.remove(name).unwrap();
    }
    // Every removed catalog stays removed across instances.
    assert!(fx.marketplaces().list().unwrap().is_empty());
    assert!(fx.marketplaces().skills(None).unwrap().is_empty());
}

#[test]
fn disabling_hides_entries_and_survives_reload() {
    let fx = Fixture::new();
    let marketplaces = fx.marketplaces();
    for &(name, _) in BUILTINS {
        marketplaces.set_enabled(name, false).unwrap();
        let infos = fx.marketplaces().list().unwrap();
        assert!(!infos.iter().find(|info| info.name == name).unwrap().enabled);
        assert!(fx.marketplaces().skills(Some(name)).unwrap().is_empty());
        assert!(!fx.marketplaces().skills(None).unwrap().is_empty());
        marketplaces.set_enabled(name, true).unwrap();
        assert!(!fx.marketplaces().skills(Some(name)).unwrap().is_empty());
    }
}

#[test]
fn add_and_remove_persist_across_instances() {
    let fx = Fixture::new();
    let index = fx.write_index("extra.json", json!([]));
    fx.marketplaces()
        .add("extra", index.to_str().unwrap())
        .unwrap();
    let names: Vec<String> = fx
        .marketplaces()
        .list()
        .unwrap()
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert_eq!(
        names,
        vec![DEFAULT_NAME, "extra", "openai-skills", "vercel-skills"]
    );
    fx.marketplaces().remove("extra").unwrap();
    assert_eq!(fx.marketplaces().list().unwrap().len(), BUILTINS.len());
}

#[test]
fn skill_entries_parse_and_reject_unsafe_urls() {
    let fx = Fixture::new();
    let good = skill_index(
        &fx,
        "skills-ok.json",
        json!([{"name": "pdf", "description": "PDF", "url": "https://example.com/org/skills.git#skills/pdf"}]),
    );
    let marketplaces = fx.marketplaces();
    marketplaces.add("skills", good.to_str().unwrap()).unwrap();
    let skills = marketplaces.skills(Some("skills")).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "pdf");
    assert!(marketplaces.entries(Some("skills")).unwrap().is_empty());

    let bad = skill_index(
        &fx,
        "skills-bad.json",
        json!([{"name": "evil", "url": "https://example.com/org/skills.git#../outside"}]),
    );
    assert!(marketplaces.add("bad", bad.to_str().unwrap()).is_err());
}

#[test]
fn registries_written_before_the_default_get_it_once() {
    let fx = Fixture::new();
    let root = fx.base.join("marketplaces");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(REGISTRY_FILE), "{\"marketplaces\": {}}\n").unwrap();
    assert_eq!(fx.marketplaces().list().unwrap().len(), BUILTINS.len());
    for &(name, _) in BUILTINS {
        fx.marketplaces().remove(name).unwrap();
    }
    assert!(fx.marketplaces().list().unwrap().is_empty());
}

#[test]
fn legacy_seeded_registry_preserves_removed_default() {
    let fx = Fixture::new();
    let root = fx.base.join("marketplaces");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join(REGISTRY_FILE),
        r#"{"seeded":true,"marketplaces":{}}"#,
    )
    .unwrap();
    let infos = fx.marketplaces().list().unwrap();
    assert_eq!(infos.len(), BUILTINS.len() - 1);
    assert!(infos.iter().all(|info| info.name != DEFAULT_NAME));
    fx.marketplaces().remove("openai-skills").unwrap();
    let infos = fx.marketplaces().list().unwrap();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].name, "vercel-skills");
}

#[test]
fn migration_preserves_disabled_and_custom_catalogs() {
    let fx = Fixture::new();
    let root = fx.base.join("marketplaces");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(REGISTRY_FILE), json!({
        "seeded": true,
        "marketplaces": {
            "anthropic-skills": {"index":"builtin", "enabled":false, "refreshed_at":null},
            "openai-skills": {"index":"https://example.com/custom.json", "enabled":false, "refreshed_at":42}
        }
    }).to_string()).unwrap();
    fx.marketplaces()
        .set_enabled("vercel-skills", false)
        .unwrap();
    let infos = fx.marketplaces().list().unwrap();
    assert_eq!(infos.len(), BUILTINS.len());
    assert!(infos.iter().all(|info| !info.enabled));
    let custom = infos
        .iter()
        .find(|info| info.name == "openai-skills")
        .unwrap();
    assert_eq!(custom.index, "https://example.com/custom.json");
    assert_eq!(custom.refreshed_at, Some(42));
    assert!(!custom.builtin);
    fx.marketplaces().remove("openai-skills").unwrap();
    assert!(
        fx.marketplaces()
            .list()
            .unwrap()
            .iter()
            .all(|info| info.name != "openai-skills")
    );
}

#[test]
fn adding_new_builtin_does_not_restore_previously_removed_catalogs() {
    let fx = Fixture::new();
    let root = fx.base.join("marketplaces");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join(REGISTRY_FILE),
        json!({
            "seeded": true,
            "seeded_builtins": [DEFAULT_NAME, "openai-skills"],
            "marketplaces": {}
        })
        .to_string(),
    )
    .unwrap();
    let infos = fx.marketplaces().list().unwrap();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].name, "vercel-skills");
    fx.marketplaces().remove("vercel-skills").unwrap();
    assert!(fx.marketplaces().list().unwrap().is_empty());
}

#[test]
fn bundled_indexes_are_valid_and_use_individual_skill_paths() {
    let mut names = BTreeSet::new();
    for &(name, text) in BUILTINS {
        validate_marketplace_name(name).unwrap();
        assert!(names.insert(name));
        let index = parse_index(text).unwrap();
        assert!(index.plugins.is_empty());
        assert!(!index.skills.is_empty());
        for skill in index.skills {
            assert!(!skill.description.trim().is_empty());
            let source = skill_source::parse(&skill.url).unwrap();
            assert!(source.url.starts_with("https://github.com/"));
            assert!(source.path.is_some());
            assert_eq!(
                skill_source::default_name(&source).as_deref(),
                Some(skill.name.as_str())
            );
        }
    }
}

#[test]
fn unknown_builtin_does_not_silently_offer_anthropic_skills() {
    let fx = Fixture::new();
    let error = fx
        .marketplaces()
        .index_text("unknown", BUILTIN_INDEX)
        .unwrap_err();
    assert!(error.to_string().contains("unknown built-in marketplace"));
}
