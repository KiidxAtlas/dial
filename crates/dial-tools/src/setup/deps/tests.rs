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

#[cfg(unix)]
#[test]
fn env_find_returns_only_executables_on_the_search_path() {
    let dir = tempfile::tempdir().unwrap();
    fake_bin(dir.path(), "tool-a");
    std::fs::write(dir.path().join("tool-b"), "data").unwrap();
    let env = Env {
        path: vec![dir.path().to_path_buf()],
        user_dir: dir.path().to_path_buf(),
    };
    assert_eq!(env.find("tool-a"), Some(dir.path().join("tool-a")));
    assert_eq!(
        env.find("tool-b"),
        None,
        "a non-executable file is not a program"
    );
    assert_eq!(env.find("missing"), None);
    assert_eq!(env.find(""), None);
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
    let program = if cfg!(windows) { "bun.exe" } else { "bun" };
    fake_bin(&path_dir, program);
    let env = Env {
        path: vec![path_dir.clone()],
        user_dir: user.clone(),
    };
    let bun = recipe("bun").unwrap();
    let from_path = || locate(bun, &env).unwrap().canonicalize().unwrap();
    assert_eq!(from_path(), path_dir.join(program).canonicalize().unwrap());

    let private = install_dir(&user, "bun");
    fake_bin(&private.join("node_modules/.bin"), "bun");
    if cfg!(windows) {
        // A leftover POSIX shim alone must not shadow a working Bun on PATH.
        assert_eq!(from_path(), path_dir.join(program).canonicalize().unwrap());
        fake_bin(&private.join("node_modules/bun/bin"), "bun.exe");
        assert_eq!(
            locate(bun, &env),
            Some(private.join("node_modules/bun/bin/bun.exe"))
        );
    } else {
        assert_eq!(
            locate(bun, &env),
            Some(private.join("node_modules/.bin/bun"))
        );
    }
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

#[test]
fn every_local_engine_recipe_is_registered() {
    use crate::llm::local_runtime::LocalEngine;
    for engine in [
        LocalEngine::Mlx,
        LocalEngine::MlxServe,
        LocalEngine::VllmMlx,
        LocalEngine::LlamaCpp,
        LocalEngine::Atomic,
        LocalEngine::IkLlama,
        LocalEngine::MistralRs,
        LocalEngine::Vllm,
        LocalEngine::Sglang,
        LocalEngine::Exl3,
    ] {
        let id = engine.recipe_id().expect("concrete engine has a recipe id");
        assert!(
            recipe(id).is_some(),
            "recipe `{id}` for {engine:?} must exist in RECIPES"
        );
    }
}
