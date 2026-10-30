use super::*;
use crate::setup::deps::{prebuilt, recipe};

/// Fake package managers: executable stubs on a fake `PATH` directory.
pub(crate) fn fake_tools(root: &Path, tools: &[&str]) -> Env {
    let bin = root.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    for tool in tools {
        let path = bin.join(tool);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
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
    assert!(plan.steps[0].program.ends_with("/fakebin/npm"));
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
    assert!(plan.steps[0].display().ends_with("brew install bun"));
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
    if prebuilt::current_platform().is_none() {
        return;
    }
    let plan = plan(recipe("node").unwrap(), &env).unwrap();
    let fetch = plan.fetch.expect("a verified download");
    assert_eq!(fetch.dest, env.user_dir.join("tools").join("node"));
    assert!(fetch.asset.url.starts_with("https://nodejs.org/dist/"));
    assert_eq!(plan.expected, fetch.dest.join("bin/node"));
    assert!(plan.steps.is_empty());
    assert!(plan.method.contains("sha256 checked"));
}

#[test]
fn npm_recipes_use_the_npm_of_a_dial_installed_node() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fake_tools(tmp.path(), &[]);
    let bin = env.user_dir.join("tools/node/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("node"), "#!/bin/sh\necho v22.23.3\n").unwrap();
    std::fs::write(bin.join("npm"), "#!/bin/sh\nexit 0\n").unwrap();
    for path in [bin.join("node"), bin.join("npm")] {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let bun = plan(recipe("bun").unwrap(), &env).unwrap();
    assert_eq!(bun.steps[0].program, bin.join("npm").display().to_string());
    let path = bun.steps[0]
        .env
        .iter()
        .find(|(key, _)| key == "PATH")
        .unwrap();
    assert!(path.1.starts_with(&bin.display().to_string()));
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
