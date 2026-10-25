use super::*;
use serde_json::json;
use std::fs;
#[cfg(unix)]
use std::time::Instant;

fn env(root: &Path) -> Env {
    Env {
        path: vec![root.join("bin")],
        user_dir: root.join("user"),
    }
}

fn executable(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn response(value: serde_json::Value) -> Query {
    Query {
        code: 0,
        stdout: value.to_string(),
        stderr: String::new(),
    }
}

fn private_bun(env: &Env, version: &str) -> PathBuf {
    let dir = install_dir(&env.user_dir, "bun");
    let path = dir.join("node_modules/.bin/bun");
    executable(&path, "#!/bin/sh\nexit 99\n");
    let manifest = dir.join("node_modules/bun/package.json");
    fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    fs::write(manifest, json!({"version": version}).to_string()).unwrap();
    path
}

#[test]
fn numeric_versions_and_llama_build_namespaces_never_authorize_a_downgrade() {
    for (id, current, latest, expected) in [
        ("bun", "1.9.0", "1.10.0", Some(Cmp::Less)),
        ("node", "v22.0", "22.0.0", Some(Cmp::Equal)),
        ("bun", "2.0.0", "1.10.0", Some(Cmp::Greater)),
        ("bun", "1.3.0-dev", "1.3.0", None),
        ("bun", "unknown", "1.3.0", None),
        (
            "llama-cpp",
            "version: 11669 (abc)",
            "b11868",
            Some(Cmp::Less),
        ),
        ("llama-cpp", "b12000", "b11868", Some(Cmp::Greater)),
        (
            "llama-cpp",
            "0.7.0-dev (build 11868, commit 78a8ae14b)",
            "b11869",
            Some(Cmp::Less),
        ),
        (
            "llama-cpp",
            "0.7.0-dev (build 11868, commit 78a8ae14b)",
            "b11867",
            Some(Cmp::Greater),
        ),
        ("llama-cpp", "v0.7.0", "b11868", None),
        ("llama-cpp", "b11868", "v0.7.0", None),
    ] {
        assert_eq!(
            compare_versions(id, current, latest),
            expected,
            "{id}: {current} vs {latest}"
        );
    }
}

#[test]
fn private_npm_checks_are_read_only_and_updates_are_pinned_to_the_same_prefix() {
    let root = tempfile::tempdir().unwrap();
    let env = env(root.path());
    let path = private_bun(&env, "1.3.0");
    executable(&env.path[0].join("npm"), "#!/bin/sh\nexit 99\n");
    let manifest = install_dir(&env.user_dir, "bun").join("node_modules/bun/package.json");
    let before = fs::read(&manifest).unwrap();
    let mut seen = Vec::new();
    let candidate = inspect(recipe("bun").unwrap(), &path, &env, &mut |step| {
        seen.push(step.clone());
        Ok(response(json!("1.4.0")))
    })
    .unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].args,
        [
            "view",
            "bun",
            "dist-tags.latest",
            "--json",
            "--fetch-retries=0",
            "--fetch-timeout=10000"
        ]
    );
    assert_eq!(fs::read(&manifest).unwrap(), before);
    assert_eq!(candidate.row.status, Status::Available);
    let plan = candidate.plan.unwrap();
    assert_eq!(plan.destination, install_dir(&env.user_dir, "bun"));
    assert!(plan.fetch.is_none());
    assert_eq!(plan.steps.len(), 1);
    let args = &plan.steps[0].args;
    assert_eq!(args[0], "install");
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--prefix", plan.destination.to_str().unwrap()])
    );
    assert!(args.iter().any(|arg| arg == "--ignore-scripts"));
    assert!(args.iter().any(|arg| arg == "bun@1.4.0"));
    assert!(!args.iter().any(|arg| arg == "-g" || arg == "--global"));
}

#[test]
fn newer_and_unknown_private_package_versions_do_not_get_an_update_plan() {
    for (current, expected) in [("9.0.0", Status::Current), ("dev-build", Status::Manual)] {
        let root = tempfile::tempdir().unwrap();
        let env = env(root.path());
        let path = private_bun(&env, current);
        executable(&env.path[0].join("npm"), "#!/bin/sh\nexit 99\n");
        let candidate = inspect(recipe("bun").unwrap(), &path, &env, &mut |_| {
            Ok(response(json!("1.4.0")))
        })
        .unwrap();
        assert_eq!(candidate.row.status, expected);
        assert!(candidate.plan.is_none());
    }
}

#[test]
fn path_tool_is_not_replaced_by_a_dial_private_install() {
    let root = tempfile::tempdir().unwrap();
    let env = env(root.path());
    let path = env.path[0].join("bun");
    executable(&path, "#!/bin/sh\nexit 99\n");
    let candidate = inspect(recipe("bun").unwrap(), &path, &env, &mut |_| {
        unreachable!("unowned binary queried")
    })
    .unwrap();
    assert_eq!(candidate.row.status, Status::Manual);
    assert!(candidate.plan.is_none());
    assert!(!env.user_dir.exists());
}

#[test]
fn brew_upgrade_requires_the_active_binary_to_belong_to_the_installed_keg() {
    for owned in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let env = env(root.path());
        executable(&env.path[0].join("brew"), "#!/bin/sh\nexit 99\n");
        executable(&env.path[0].join("curl"), "#!/bin/sh\nexit 99\n");
        let cellar = root.path().join("Cellar/llama.cpp");
        fs::create_dir_all(&cellar).unwrap();
        let path = if owned {
            cellar.join("0.6.0/bin/llama-server")
        } else {
            env.path[0].join("llama-server")
        };
        executable(&path, "#!/bin/sh\nexit 99\n");
        let mut seen = Vec::new();
        let candidate = inspect(recipe("llama-cpp").unwrap(), &path, &env, &mut |step| {
            seen.push(step.clone());
            if step.args[0] == "info" {
                Ok(response(json!({"formulae": [{"installed": [{"version": "0.6.0"}], "versions": {"stable": "0.7.0"}}]})))
            } else if step.args[0] == "--cellar" {
                assert_eq!(step.args, ["--cellar", "llama.cpp"]);
                Ok(Query { code: 0, stdout: cellar.display().to_string(), stderr: String::new() })
            } else {
                assert_eq!(step.program, env.path[0].join("curl").to_string_lossy());
                assert!(step.args.iter().any(|arg| arg.starts_with("https://formulae.brew.sh/")));
                Ok(response(json!({"versions": {"stable": "0.7.0"}})))
            }
        }).unwrap();
        assert_eq!(seen[0].args, ["info", "--json=v2", "llama.cpp"]);
        for step in seen {
            assert!(
                step.env
                    .contains(&("HOMEBREW_NO_AUTO_UPDATE".into(), "1".into()))
            );
        }
        assert_eq!(
            candidate.row.status,
            if owned {
                Status::Available
            } else {
                Status::Manual
            }
        );
        assert_eq!(candidate.plan.is_some(), owned);
        if let Some(plan) = candidate.plan {
            assert_eq!(plan.steps[0].args, ["upgrade", "llama.cpp"]);
        }
    }
}

#[test]
fn newer_direct_llama_build_is_retained_without_using_an_older_pinned_archive() {
    let root = tempfile::tempdir().unwrap();
    let env = env(root.path());
    let path = install_dir(&env.user_dir, "llama-cpp").join("llama-server");
    executable(&path, "#!/bin/sh\nexit 99\n");
    executable(&env.path[0].join("curl"), "#!/bin/sh\nexit 99\n");
    let candidate = inspect(recipe("llama-cpp").unwrap(), &path, &env, &mut |step| {
        if step.program == path.to_string_lossy() {
            assert_eq!(step.args, ["--version"]);
            Ok(Query {
                code: 0,
                stdout: "0.7.0-dev (build 999999, commit 78a8ae14b)\n".into(),
                stderr: String::new(),
            })
        } else {
            assert!(step.args.iter().any(
                |arg| arg == "https://api.github.com/repos/ggml-org/llama.cpp/releases/latest"
            ));
            assert!(!step.args.iter().any(|arg| arg == "-o" || arg == "--output"));
            Ok(response(
                json!({"draft": false, "prerelease": false, "tag_name": "b11868"}),
            ))
        }
    })
    .unwrap();
    assert_eq!(candidate.row.status, Status::Current);
    assert!(candidate.plan.is_none());
}

#[test]
fn query_failures_and_bad_metadata_are_not_treated_as_current() {
    let root = tempfile::tempdir().unwrap();
    let env = env(root.path());
    let path = private_bun(&env, "1.3.0");
    executable(&env.path[0].join("npm"), "#!/bin/sh\nexit 99\n");
    for query in [
        Query {
            code: 1,
            stdout: String::new(),
            stderr: "registry offline".into(),
        },
        Query {
            code: 0,
            stdout: "not json".into(),
            stderr: String::new(),
        },
    ] {
        let mut query = Some(query);
        assert!(
            inspect(recipe("bun").unwrap(), &path, &env, &mut |_| Ok(query
                .take()
                .unwrap()))
            .is_err()
        );
    }
}

#[test]
fn empty_checks_and_unreviewed_updates_do_not_create_installations() {
    let root = tempfile::tempdir().unwrap();
    let env = env(root.path());
    let cancel = AtomicBool::new(false);
    let report = check(&env, &cancel).unwrap();
    assert!(
        report
            .rows
            .iter()
            .all(|row| row.id == "browser-runtime" && row.status == Status::Manual),
        "{report:?}"
    );
    let error = update("bun", &env, &cancel, &mut |_| {
        panic!("unreviewed install started")
    })
    .unwrap_err();
    assert!(error.to_string().contains("review"), "{error:#}");
    assert!(!env.user_dir.exists());
    assert!(check(&env, &AtomicBool::new(true)).is_err());
}

#[cfg(unix)]
#[test]
fn real_read_only_query_timeout_and_cancellation_are_bounded() {
    let step = Step {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "sleep 30".into()],
        env: vec![],
    };
    let before = Instant::now();
    let error =
        query_with_timeout(&step, &AtomicBool::new(false), Duration::from_millis(100)).unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error:#}");
    assert!(before.elapsed() < Duration::from_secs(3));
    let cancel = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            cancel.store(true, Ordering::Release);
        });
        let before = Instant::now();
        let error = query_with_timeout(&step, &cancel, Duration::from_secs(5)).unwrap_err();
        assert!(error.to_string().contains("cancelled"), "{error:#}");
        assert!(before.elapsed() < Duration::from_secs(3));
    });
}

#[test]
fn mlx_checks_use_embedded_compatibility_pins_and_retain_newer_packages() {
    let manifest: toml::Value = toml::from_str(PYPROJECT).unwrap();
    let mut compatible = serde_json::Map::new();
    for dep in manifest["project"]["dependencies"].as_array().unwrap() {
        if let Some((name, version)) = dep.as_str().unwrap().split_once("==") {
            compatible.insert(name.split('[').next().unwrap().into(), json!(version));
        }
    }
    compatible.insert("rich".into(), json!("14.1.0"));
    for (version, status) in [
        (None, Status::Current),
        (Some("0.0.0"), Status::Available),
        (Some("999.0.0"), Status::Manual),
    ] {
        let root = tempfile::tempdir().unwrap();
        let env = env(root.path());
        let path = env.user_dir.join("python/.venv/bin/python");
        executable(&path, "#!/bin/sh\nexit 99\n");
        fs::write(
            env.user_dir.join("python/.venv/pyvenv.cfg"),
            "home = /test/python\n",
        )
        .unwrap();
        executable(&env.path[0].join("uv"), "#!/bin/sh\nexit 99\n");
        let private_manifest = env.user_dir.join("python/pyproject.toml");
        fs::write(
            &private_manifest,
            "# old manifest retained until approval\n",
        )
        .unwrap();
        let before = fs::read(&private_manifest).unwrap();
        let mut packages = compatible.clone();
        if let Some(version) = version {
            packages.insert("mlx-lm".into(), json!(version));
        }
        let candidate = inspect(recipe("mlx-runtime").unwrap(), &path, &env, &mut |step| {
            assert_eq!(step.program, path.to_string_lossy());
            assert_eq!(&step.args[..3], ["-I", "-B", "-c"]);
            assert!(step.args[3].contains("importlib.metadata"));
            Ok(response(json!(packages)))
        })
        .unwrap();
        assert_eq!(candidate.row.status, status);
        assert_eq!(fs::read(&private_manifest).unwrap(), before);
        assert_eq!(candidate.plan.is_some(), status == Status::Available);
        if let Some(plan) = candidate.plan {
            assert_eq!(plan.destination, env.user_dir.join("python"));
            assert_eq!(
                plan.steps[0].args,
                [
                    "sync",
                    "--project",
                    plan.destination.to_str().unwrap(),
                    "--no-dev"
                ]
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn private_package_manifest_cannot_escape_into_an_external_installation() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let env = env(root.path());
    let path = private_bun(&env, "1.3.0");
    executable(&env.path[0].join("npm"), "#!/bin/sh\nexit 99\n");
    let manifest = install_dir(&env.user_dir, "bun").join("node_modules/bun/package.json");
    let external = root.path().join("external-package.json");
    fs::write(&external, json!({"version": "1.0.0"}).to_string()).unwrap();
    fs::remove_file(&manifest).unwrap();
    symlink(&external, &manifest).unwrap();
    let error = inspect(recipe("bun").unwrap(), &path, &env, &mut |_| {
        unreachable!("escaped package queried")
    })
    .unwrap_err();
    assert!(error.to_string().contains("escapes"), "{error:#}");
}

#[cfg(unix)]
#[test]
fn failed_new_preview_revokes_an_older_review_before_explicit_update() {
    let root = tempfile::tempdir().unwrap();
    let env = env(root.path());
    private_bun(&env, "1.3.0");
    let npm = env.path[0].join("npm");
    executable(&npm, "#!/bin/sh\nprintf '\"1.4.0\"'\n");
    let text = preview("bun", &env).unwrap();
    assert!(text.contains("bun@1.4.0"), "{text}");
    executable(
        &npm,
        "#!/bin/sh\nprintf 'registry unavailable' >&2\nexit 1\n",
    );
    assert!(preview("bun", &env).is_err());
    let error = update("bun", &env, &AtomicBool::new(false), &mut |_| {
        panic!("stale review used")
    })
    .unwrap_err();
    assert!(error.to_string().contains("review"), "{error:#}");
}

#[test]
fn brew_alias_queries_the_canonical_core_formula_name() {
    let root = tempfile::tempdir().unwrap();
    let env = env(root.path());
    executable(&env.path[0].join("brew"), "#!/bin/sh\nexit 99\n");
    executable(&env.path[0].join("curl"), "#!/bin/sh\nexit 99\n");
    let cellar = root.path().join("Cellar/whisper.cpp");
    let path = cellar.join("1.0.0/bin/whisper-cli");
    executable(&path, "#!/bin/sh\nexit 99\n");
    let mut queried_api = false;
    let candidate = inspect(recipe("whisper-cpp").unwrap(), &path, &env, &mut |step| {
        if step.args[0] == "info" {
            assert_eq!(step.args, ["info", "--json=v2", "whisper-cpp"]);
            Ok(response(json!({"formulae": [{"name": "whisper.cpp", "oldnames": ["whisper-cpp"], "tap": "homebrew/core", "installed": [{"version": "1.0.0"}], "versions": {"stable": "1.0.0"}}]})))
        } else if step.args[0] == "--cellar" {
            Ok(Query { code: 0, stdout: cellar.display().to_string(), stderr: String::new() })
        } else {
            assert!(step.args.iter().any(|arg| arg == "https://formulae.brew.sh/api/formula/whisper.cpp.json"), "{:?}", step.args);
            assert!(!step.args.iter().any(|arg| arg.ends_with("/whisper-cpp.json")));
            queried_api = true;
            Ok(response(json!({"versions": {"stable": "1.0.0"}})))
        }
    }).unwrap();
    assert!(queried_api);
    assert_eq!(candidate.row.status, Status::Current);
    assert!(candidate.plan.is_none());
}

#[test]
fn brew_installed_revision_suffix_matches_the_full_keg_and_compares_revisions() {
    for (revision, expected) in [(1, Status::Current), (2, Status::Available)] {
        let root = tempfile::tempdir().unwrap();
        let env = env(root.path());
        executable(&env.path[0].join("brew"), "#!/bin/sh\nexit 99\n");
        executable(&env.path[0].join("curl"), "#!/bin/sh\nexit 99\n");
        let cellar = root.path().join("Cellar/node");
        let path = cellar.join("26.12.1_1/bin/node");
        executable(&path, "#!/bin/sh\nexit 99\n");
        let candidate = inspect(recipe("node").unwrap(), &path, &env, &mut |step| {
            if step.args[0] == "info" {
                Ok(response(json!({"formulae": [{"name": "node", "tap": "homebrew/core", "installed": [{"version": "26.12.1_1"}], "revision": revision, "versions": {"stable": "26.12.1"}}]})))
            } else if step.args[0] == "--cellar" {
                Ok(Query { code: 0, stdout: cellar.display().to_string(), stderr: String::new() })
            } else {
                Ok(response(json!({"versions": {"stable": "26.12.1"}, "revision": revision})))
            }
        }).unwrap();
        assert_eq!(candidate.row.status, expected, "{}", candidate.row.detail);
        assert_eq!(candidate.plan.is_some(), expected == Status::Available);
        if let Some(plan) = candidate.plan {
            assert_eq!(plan.expected, path);
            assert_eq!(plan.steps[0].args, ["upgrade", "node"]);
        }
    }
}

#[test]
fn brew_third_party_tap_stays_manual_without_querying_the_core_api() {
    let root = tempfile::tempdir().unwrap();
    let env = env(root.path());
    executable(&env.path[0].join("brew"), "#!/bin/sh\nexit 99\n");
    executable(&env.path[0].join("curl"), "#!/bin/sh\nexit 99\n");
    let cellar = root.path().join("Cellar/llama.cpp");
    let path = cellar.join("0.6.0/bin/llama-server");
    executable(&path, "#!/bin/sh\nexit 99\n");
    let mut queries = 0;
    let candidate = inspect(recipe("llama-cpp").unwrap(), &path, &env, &mut |step| {
        queries += 1;
        if step.args[0] == "info" {
            Ok(response(json!({"formulae": [{"name": "llama.cpp", "tap": "third-party/tools", "installed": [{"version": "0.6.0"}], "versions": {"stable": "0.7.0"}}]})))
        } else {
            assert_eq!(step.args, ["--cellar", "llama.cpp"], "third-party tap must not query official API");
            Ok(Query { code: 0, stdout: cellar.display().to_string(), stderr: String::new() })
        }
    }).unwrap();
    assert_eq!(queries, 2);
    assert_eq!(candidate.row.status, Status::Manual);
    assert!(candidate.plan.is_none());
}
