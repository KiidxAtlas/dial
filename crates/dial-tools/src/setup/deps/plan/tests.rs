use super::*;
use crate::setup::deps::{prebuilt, recipe};

fn manager_name(tool: &str) -> String {
    if cfg!(windows) {
        format!("{tool}.cmd")
    } else {
        tool.to_owned()
    }
}

/// Fake package managers: executable stubs on a fake `PATH` directory.
pub(crate) fn fake_tools(root: &Path, tools: &[&str]) -> Env {
    let bin = root.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    for tool in tools {
        let path = bin.join(manager_name(tool));
        std::fs::write(
            &path,
            if cfg!(windows) {
                "@exit /b 0\r\n"
            } else {
                "#!/bin/sh\nexit 0\n"
            },
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    Env {
        path: vec![bin],
        user_dir: root.join("dial"),
    }
}

#[test]
fn bun_prefers_npm_into_dial_dir_and_never_uses_sudo() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fake_tools(tmp.path(), &["npm", "brew"]);
    let plan = plan(recipe("bun").unwrap(), &env).unwrap();
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(
        std::fs::canonicalize(&plan.steps[0].program).unwrap(),
        env.path[0]
            .join(manager_name("npm"))
            .canonicalize()
            .unwrap()
    );
    assert!(plan.steps[0].args.contains(&"bun".to_owned()));
    assert!(plan.destination.starts_with(env.user_dir.join("tools")));
    assert!(plan.fetch.is_none());
    assert!(
        plan.steps
            .iter()
            .all(|step| !step.program.ends_with("sudo"))
    );
}

#[test]
fn falls_back_to_brew_when_npm_is_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fake_tools(tmp.path(), &["brew"]);
    let plan = plan(recipe("bun").unwrap(), &env).unwrap();
    assert_eq!(
        std::fs::canonicalize(&plan.steps[0].program).unwrap(),
        env.path[0]
            .join(manager_name("brew"))
            .canonicalize()
            .unwrap()
    );
    assert_eq!(plan.steps[0].args, ["install", "bun"]);
}

#[test]
fn uv_tools_install_under_dial_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fake_tools(tmp.path(), &["uv"]);
    let plan = plan(recipe("mlx-whisper").unwrap(), &env).unwrap();
    let step = &plan.steps[0];
    assert_eq!(step.args, vec!["tool", "install", "mlx-whisper"]);
    assert!(
        step.env
            .iter()
            .all(|(_, value)| value.starts_with(&env.user_dir.display().to_string()))
    );
}

#[test]
fn no_package_manager_is_reported_plainly() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fake_tools(tmp.path(), &[]);
    // Bun can be downloaded on supported platforms, so only the package managers that
    // remain are named when nothing is on PATH.
    let blocked = plan(recipe("gopls").unwrap(), &env);
    assert_eq!(blocked, Err(Blocked::NoPackageManager(vec!["brew", "go"])));
    if prebuilt::current_platform().is_some() {
        assert!(plan(recipe("bun").unwrap(), &env).is_ok());
    }
}

#[test]
fn private_bun_download_and_piper_launcher_are_discoverable() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fake_tools(tmp.path(), &[]);
    let exe = std::env::consts::EXE_SUFFIX;
    for (id, rel) in [
        ("piper", format!("bin/piper{exe}")),
        ("bun", format!("bun{exe}")),
    ] {
        let recipe = recipe(id).unwrap();
        let expected = install_dir(&env.user_dir, id).join(rel);
        if id == "bun" {
            let Some(asset) = prebuilt::current_platform().and_then(|p| prebuilt::BUN.asset(p))
            else {
                continue;
            };
            let plan = plan(recipe, &env).unwrap();
            assert_eq!(plan.fetch.unwrap().asset, *asset);
            assert_eq!(plan.expected, expected);
        }
        assert!(locate(recipe, &env).is_none());
        std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
        std::fs::write(&expected, b"fixture").unwrap();
        assert_eq!(locate(recipe, &env), Some(expected));
    }
}

#[test]
fn go_and_rustup_recipes_plan_into_the_expected_places() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fake_tools(tmp.path(), &["go", "rustup"]);
    let gopls = plan(recipe("gopls").unwrap(), &env).unwrap();
    assert_eq!(gopls.steps[0].args[0], "install");
    assert!(gopls.steps[0].env.is_empty());
    assert!(gopls.expected.as_os_str().is_empty());
    assert!(!gopls.destination.starts_with(&env.user_dir));
    let ra = plan(recipe("rust-analyzer").unwrap(), &env).unwrap();
    assert_eq!(ra.steps[0].args, vec!["component", "add", "rust-analyzer"]);
    assert!(ra.expected.as_os_str().is_empty());
}

#[test]
fn manual_recipes_return_their_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fake_tools(tmp.path(), &["npm"]);
    match plan(recipe("hindsight").unwrap(), &env) {
        Err(Blocked::Manual(hint)) => assert!(hint.contains("Docker")),
        other => panic!("expected manual hint, got {other:?}"),
    }
}

#[test]
fn node_downloads_into_dial_dir_when_no_package_manager_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fake_tools(tmp.path(), &[]);
    let Some(asset) = prebuilt::current_platform().and_then(|p| prebuilt::NODE.asset(p)) else {
        assert!(plan(recipe("node").unwrap(), &env).is_err());
        return;
    };
    let plan = plan(recipe("node").unwrap(), &env).unwrap();
    let fetch = plan.fetch.expect("a verified download");
    assert_eq!(fetch.dest, env.user_dir.join("tools").join("node"));
    assert!(
        fetch.asset.url.starts_with("https://nodejs.org/dist/")
            || fetch
                .asset
                .url
                .starts_with("https://unofficial-builds.nodejs.org/download/release/")
    );
    assert_eq!(fetch.asset, *asset);
    assert_eq!(plan.expected, fetch.dest.join(prebuilt::NODE.binary));
    assert!(plan.steps.is_empty());
    assert!(plan.method.contains("sha256 checked"));
}

#[test]
fn npm_recipes_use_the_npm_of_a_dial_installed_node() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fake_tools(tmp.path(), &[]);
    let node = env.user_dir.join("tools/node").join(prebuilt::NODE.binary);
    let bin = node.parent().unwrap();
    let npm = bin.join(manager_name("npm"));
    std::fs::create_dir_all(bin).unwrap();
    std::fs::write(&node, b"node fixture").unwrap();
    std::fs::write(&npm, b"npm fixture").unwrap();
    #[cfg(unix)]
    for path in [&node, &npm] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let bun = plan(recipe("bun").unwrap(), &env).unwrap();
    assert_eq!(
        std::fs::canonicalize(&bun.steps[0].program).unwrap(),
        npm.canonicalize().unwrap()
    );
    let path = bun.steps[0]
        .env
        .iter()
        .find(|(key, _)| key == "PATH")
        .unwrap();
    // Compare the first PATH entry as a file, not as text: Windows may spell the same
    // directory with other separators or a short (8.3) temp-directory name.
    let first = std::env::split_paths(&path.1).next().unwrap();
    assert_eq!(first.canonicalize().unwrap(), bin.canonicalize().unwrap());
    // Language servers must not acquire an implicit prefix under Dial's downloaded Node.
    assert_eq!(
        plan(recipe("pyright-langserver").unwrap(), &env),
        Err(Blocked::NoPackageManager(vec!["npm"]))
    );
}

#[test]
fn language_servers_leave_package_manager_destinations_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fake_tools(tmp.path(), &["npm", "uv"]);
    for (id, args) in [
        (
            "pyright-langserver",
            vec!["install", "--global", "--no-audit", "--no-fund", "pyright"],
        ),
        ("pylsp", vec!["tool", "install", "python-lsp-server"]),
    ] {
        let planned = plan(recipe(id).unwrap(), &env).unwrap();
        assert_eq!(planned.steps[0].args, args, "{id}");
        assert!(
            planned.steps[0].env.iter().all(|(key, _)| key == "PATH"),
            "{id}"
        );
        assert!(planned.fetch.is_none(), "{id}");
        assert!(!planned.destination.starts_with(&env.user_dir), "{id}");
        assert!(planned.expected.as_os_str().is_empty(), "{id}");
    }
}
