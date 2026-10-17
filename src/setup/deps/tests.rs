use super::*;

fn fake_bin(dir: &Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn registry_ids_are_unique_and_requirements_resolve() {
    let mut ids = std::collections::BTreeSet::new();
    for recipe in RECIPES {
        assert!(ids.insert(recipe.id), "duplicate id {}", recipe.id);
        for required in recipe.requires {
            assert!(
                recipe_exists(required),
                "{} requires missing {required}",
                recipe.id
            );
        }
        assert!(!recipe.methods.is_empty(), "{} has no method", recipe.id);
    }
}

fn recipe_exists(id: &str) -> bool {
    recipe(id).is_some()
}

#[test]
fn install_dir_lives_under_user_dir() {
    let dir = install_dir(Path::new("/home/x/.dial"), "bun");
    assert_eq!(dir, Path::new("/home/x/.dial/tools/bun"));
}

#[test]
fn locate_prefers_dial_install_then_path() {
    let tmp = tempfile::tempdir().unwrap();
    let user = tmp.path().join("user");
    let path_dir = tmp.path().join("path");
    fake_bin(&path_dir, "bun");
    let env = Env {
        path: vec![path_dir.clone()],
        user_dir: user.clone(),
    };
    let bun = recipe("bun").unwrap();
    assert_eq!(locate(bun, &env), Some(path_dir.join("bun")));

    fake_bin(&install_dir(&user, "bun").join("node_modules/.bin"), "bun");
    assert_eq!(
        locate(bun, &env),
        Some(install_dir(&user, "bun").join("node_modules/.bin/bun"))
    );
}

#[test]
fn report_marks_missing_and_manual_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let env = Env {
        path: Vec::new(),
        user_dir: tmp.path().join("user"),
    };
    let rows = report(&env);
    let hindsight = rows.iter().find(|row| row.id == "hindsight").unwrap();
    assert_eq!(hindsight.status, Status::Missing);
    assert!(hindsight.detail.contains("Docker"));
    assert!(rows.iter().all(|row| row.status == Status::Missing));
}

#[test]
fn settings_map_to_config_keys() {
    let path = Path::new("/x/bun");
    assert!(settings_for("bun", path).is_empty());
    assert_eq!(
        settings_for("gopls", path),
        vec![("lsp.gopls.program".into(), "/x/bun".into())]
    );
    assert_eq!(
        settings_for("node", path),
        vec![("eval.node".into(), "/x/bun".into())]
    );
}
