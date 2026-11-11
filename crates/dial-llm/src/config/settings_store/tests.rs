use super::*;
use crate::config::schema;
fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Layout) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    (home, project, layout)
}

#[test]
fn home_workspace_loads_and_saves_existing_user_configuration() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(home.path(), home.path().join(".dial")).unwrap();
    save(
        &layout,
        Scope::Global,
        &[Change::Set {
            path: "model".into(),
            value: "home-model".into(),
        }],
    )
    .unwrap();
    let loaded = crate::config::types::Config::load(&layout, None).unwrap();
    assert_eq!(loaded.model, "home-model");
    save(
        &layout,
        Scope::Project,
        &[Change::Set {
            path: "model".into(),
            value: "project-model".into(),
        }],
    )
    .unwrap();
    let loaded = crate::config::types::Config::load(&layout, None).unwrap();
    assert_eq!(loaded.model, "project-model");
}

#[test]
fn prefer_subagents_persists_and_reloads_with_project_precedence() {
    let (_home, _project, layout) = fixture();
    let overlay = toml::Table::new();
    let initial = load_config(&layout, &overlay).unwrap();
    assert!(!initial.task.prefer_subagents);
    let mut reload = SettingsReload::new(&layout, &overlay, &initial);
    for (scope, enabled) in [(Scope::Global, true), (Scope::Project, false)] {
        let saved = save_config(
            &layout,
            scope,
            &[Change::Set {
                path: "task.prefer_subagents".into(),
                value: toml::Value::Boolean(enabled),
            }],
        )
        .unwrap();
        assert_eq!(saved.task.prefer_subagents, enabled);
        assert_eq!(
            load_config(&layout, &overlay)
                .unwrap()
                .task
                .prefer_subagents,
            enabled
        );
        let (current, diagnostics) = reload.reload(&layout, &overlay, None);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(current.task.prefer_subagents, enabled);
        assert_eq!(reload.config().task.prefer_subagents, enabled);
    }
    save_config(
        &layout,
        Scope::Project,
        &[Change::Remove {
            path: "task.prefer_subagents".into(),
        }],
    )
    .unwrap();
    let (current, diagnostics) = reload.reload(&layout, &overlay, None);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert!(
        current.task.prefer_subagents,
        "removing project override restores global preference"
    );
}

#[test]
fn model_assignment_rejects_invalid_project_without_changing_global() {
    let (_home, _project, layout) = fixture();
    save_config(
        &layout,
        Scope::Global,
        &[Change::Set {
            path: "model".into(),
            value: "original".into(),
        }],
    )
    .unwrap();
    let original = fs::read(&layout.config_files()[0]).unwrap();
    save_config(&layout, Scope::Project, &[]).unwrap();
    write_private(&layout.config_files()[1], b"not valid TOML [").unwrap();
    assert!(
        save_model_config(
            &layout,
            Scope::Global,
            &[Change::Set {
                path: "model".into(),
                value: "replacement".into(),
            }],
            "main"
        )
        .is_err()
    );
    assert_eq!(fs::read(&layout.config_files()[0]).unwrap(), original);
}

#[test]
fn malformed_project_hooks_reject_save_without_changing_files() {
    let (_home, _project, layout) = fixture();
    save_config(&layout, Scope::Global, &[]).unwrap();
    save_config(&layout, Scope::Project, &[]).unwrap();
    let files = layout.config_files();
    let original = files.each_ref().map(|path| fs::read(path).unwrap());
    let result = save_config(
        &layout,
        Scope::Project,
        &[Change::Set {
            path: "hooks".into(),
            value: "invalid".into(),
        }],
    );
    assert!(result.is_err(), "malformed hooks must reject the save");
    assert_eq!(
        files.each_ref().map(|path| fs::read(path).unwrap()),
        original
    );
}

#[test]
fn project_override_cannot_mask_invalid_global_save_or_preview() {
    let (_home, _project, layout) = fixture();
    save_config(&layout, Scope::Global, &[]).unwrap();
    save_config(
        &layout,
        Scope::Project,
        &[Change::Set {
            path: "preparation.lsp_paths".into(),
            value: toml::Value::Array(vec![]),
        }],
    )
    .unwrap();
    let files = layout.config_files();
    let original = files.each_ref().map(|path| fs::read(path).unwrap());
    let changes = [Change::Set {
        path: "preparation.lsp_paths".into(),
        value: toml::Value::Array(vec!["../outside.rs".into()]),
    }];
    let preview = preview_config(&layout, Scope::Global, &changes, &toml::Table::new());
    let save = save_config(&layout, Scope::Global, &changes);
    assert!(
        preview.is_err() && save.is_err(),
        "invalid global paths must reject both preview and save: preview rejected={}, save rejected={}",
        preview.is_err(),
        save.is_err()
    );
    assert_eq!(
        files.each_ref().map(|path| fs::read(path).unwrap()),
        original
    );
}

#[test]
fn valid_global_save_preserves_project_override() {
    let (_home, _project, layout) = fixture();
    save_config(
        &layout,
        Scope::Project,
        &[Change::Set {
            path: "preparation.lsp_paths".into(),
            value: toml::Value::Array(vec![]),
        }],
    )
    .unwrap();
    let changes = [Change::Set {
        path: "preparation.lsp_paths".into(),
        value: toml::Value::Array(vec!["src/lib.rs".into()]),
    }];
    let preview = preview_config(&layout, Scope::Global, &changes, &toml::Table::new()).unwrap();
    let saved = save_config(&layout, Scope::Global, &changes).unwrap();
    assert!(preview.preparation.lsp_paths.is_empty());
    assert!(saved.preparation.lsp_paths.is_empty());
    assert_eq!(
        load_layer(&layout, Scope::Global).unwrap()["preparation"]["lsp_paths"],
        toml::Value::Array(vec!["src/lib.rs".into()])
    );
}

#[test]
fn another_sessions_model_write_does_not_retarget_a_pinned_session() {
    let (_home, _project, layout) = fixture();
    let set = |path: &str, value: &str| Change::Set {
        path: path.into(),
        value: value.into(),
    };
    save_config(
        &layout,
        Scope::Global,
        &[set("model", "mine"), set("service_tier", "flex")],
    )
    .unwrap();
    let overlay = toml::Table::new();
    let mut pin = SessionModels::capture(&load_merged(&layout, &overlay).unwrap());
    // Another terminal picks a model and edits an unrelated setting.
    save_model_config(&layout, Scope::Global, &[set("model", "theirs")], "main").unwrap();
    save_config(&layout, Scope::Global, &[set("service_tier", "priority")]).unwrap();
    let config = load_session_config(&layout, &overlay, Some(&pin)).unwrap();
    assert_eq!(config.model, "mine");
    assert_eq!(config.service_tier, "priority");
    let mut reload = SettingsReload::new(&layout, &overlay, &config);
    assert_eq!(reload.reload(&layout, &overlay, Some(&pin)).0.model, "mine");
    // This session's own choice applies here and becomes the saved default.
    save_model_config(&layout, Scope::Global, &[set("model", "next")], "main").unwrap();
    pin.adopt_role(&load_merged(&layout, &overlay).unwrap(), "main");
    assert_eq!(
        load_session_config(&layout, &overlay, Some(&pin))
            .unwrap()
            .model,
        "next"
    );
    assert_eq!(load_config(&layout, &overlay).unwrap().model, "next");
}

#[test]
fn model_favorites_persist_and_project_empty_list_overrides_global() {
    use crate::llm::model_config::ModelSelection;
    let (_home, _project, layout) = fixture();
    let favorite = ModelSelection {
        provider: "openai".into(),
        model: "favorite-model".into(),
        base_url: Some("https://api.openai.com/v1".into()),
        ..ModelSelection::default()
    };
    let changes = [Change::Set {
        path: "model_favorites".into(),
        value: toml::Value::try_from(vec![favorite.clone()]).unwrap(),
    }];
    let saved = save_config(&layout, Scope::Global, &changes).unwrap();
    assert_eq!(saved.model_favorites, vec![favorite.clone()]);
    let reloaded = crate::config::types::Config::from_table(
        &layout,
        load_layer(&layout, Scope::Global).unwrap(),
    )
    .unwrap();
    assert_eq!(reloaded.model_favorites, vec![favorite.clone()]);
    let saved = save_config(
        &layout,
        Scope::Project,
        &[Change::Set {
            path: "model_favorites".into(),
            value: toml::Value::Array(vec![]),
        }],
    )
    .unwrap();
    assert!(saved.model_favorites.is_empty());
    let global: Vec<ModelSelection> =
        load_layer(&layout, Scope::Global).unwrap()["model_favorites"]
            .clone()
            .try_into()
            .unwrap();
    assert_eq!(global, vec![favorite]);
}

#[test]
fn patches_preserve_disjoint_keys_and_fail_closed() {
    let (_h, _p, l) = fixture();
    fs::write(
        l.config_files()[0].clone(),
        "unrelated = 42\n[model]\nbase_url = 'http://localhost:1234'\nname = 'old'\n",
    )
    .unwrap();
    save(
        &l,
        Scope::Global,
        &[
            Change::Set {
                path: "ui.theme".into(),
                value: toml::Value::String("light".into()),
            },
            Change::Remove {
                path: "model.base_url".into(),
            },
        ],
    )
    .unwrap();
    let text = fs::read_to_string(&l.config_files()[0]).unwrap();
    let parsed: toml::Table = text.parse().unwrap();
    assert_eq!(parsed["unrelated"].as_integer(), Some(42));
    assert!(parsed["model"].get("base_url").is_none());
    assert_eq!(parsed["model"]["name"].as_str(), Some("old"));
    fs::write(&l.config_files()[0], "[broken").unwrap();
    assert!(save(&l, Scope::Global, &[]).is_err());
    assert_eq!(fs::read_to_string(&l.config_files()[0]).unwrap(), "[broken");
}
#[test]
fn history_roundtrips_and_preserves_external_appends() {
    let (_h, _p, l) = fixture();
    load_history(&l, 5).unwrap();
    save_history(&l, &["a\n雪".into()], 5).unwrap();
    fs::write(
        history_path(&l).unwrap(),
        serde_json::to_vec(&vec!["a\n雪", "external"]).unwrap(),
    )
    .unwrap();
    save_history(&l, &["a\n雪".into(), "new".into(), "new".into()], 5).unwrap();
    assert_eq!(load_history(&l, 5).unwrap(), ["a\n雪", "external", "new"]);
    #[cfg(unix)]
    assert_eq!(
        platform_fs::mode(&fs::metadata(history_path(&l).unwrap()).unwrap()) & 0o777,
        0o600
    );
    #[cfg(windows)]
    assert!(platform_fs::is_owner_only(&File::open(history_path(&l).unwrap()).unwrap()).unwrap());
    save_history(&l, &["external".into(), "new".into(), "last".into()], 2).unwrap();
    assert_eq!(load_history(&l, 2).unwrap(), ["new", "last"]);
}
#[test]
fn refuses_symlink_and_hardlink_targets() {
    let (_h, _p, l) = fixture();
    let outside = tempfile::NamedTempFile::new().unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), &l.config_files()[0]).unwrap();
        assert!(save(&l, Scope::Global, &[]).is_err());
        fs::remove_file(&l.config_files()[0]).unwrap();
    }
    fs::hard_link(outside.path(), &l.config_files()[0]).unwrap();
    assert!(save(&l, Scope::Global, &[]).is_err());
}

#[test]
fn stale_insert_refuses_the_whole_batch_and_preserves_existing_bytes() {
    let (_home, _project, layout) = fixture();
    let install = |program: &str| Change::Insert {
        path: "mcp.notes".into(),
        value: toml::Value::try_from(crate::config::types::McpServerConfig {
            program: program.into(),
            ..Default::default()
        })
        .unwrap(),
    };
    let stale = install("stale-server");
    save_config(&layout, Scope::Global, &[install("winner-server")]).unwrap();
    let before = fs::read(&layout.config_files()[0]).unwrap();
    let error = save_config(
        &layout,
        Scope::Global,
        &[
            Change::Set {
                path: "model".into(),
                value: "must-not-publish".into(),
            },
            stale,
        ],
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("already configured"));
    assert_eq!(fs::read(&layout.config_files()[0]).unwrap(), before);
    assert!(save_config(&layout, Scope::Project, &[install("project-racer")]).is_err());
    assert!(!layout.config_files()[1].exists());
}

#[test]
fn concurrent_inserts_have_exactly_one_winner() {
    let (_home, _project, layout) = fixture();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let handles: Vec<_> = [Scope::Global, Scope::Project]
            .into_iter()
            .map(|layer| {
                let barrier = &barrier;
                let layout = &layout;
                scope.spawn(move || {
                    barrier.wait();
                    save(
                        layout,
                        layer,
                        &[Change::Insert {
                            path: "mcp.notes".into(),
                            value: toml::Value::Table(Default::default()),
                        }],
                    )
                    .is_ok()
                })
            })
            .collect();
        assert_eq!(
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(|won| *won)
                .count(),
            1
        );
    });
}

// Release the held lock even on failure, so a regression fails instead of hanging the suite.
fn assert_lock_wait_is_bounded(path: &Path, operation: impl FnOnce() -> Result<()> + Send) {
    let lock = locked(path).unwrap();
    std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        scope.spawn(move || {
            let _ = tx.send(operation());
        });
        let result = rx.recv_timeout(Duration::from_secs(1));
        drop(lock);
        assert!(
            result.is_ok(),
            "interactive storage operation waited for an occupied lock"
        );
        assert!(
            result.unwrap().is_err(),
            "an occupied record must not be modified"
        );
    });
}

#[test]
fn command_settings_and_history_do_not_wait_for_occupied_records() {
    let (_home, _project, layout) = fixture();
    let changes = [Change::Set {
        path: "experimental.turn_optimizer".into(),
        value: true.into(),
    }];
    for scope in [Scope::Global, Scope::Project] {
        let path = &layout.config_files()[usize::from(scope == Scope::Project)];
        assert_lock_wait_is_bounded(path, || save_config(&layout, scope, &changes).map(|_| ()));
        assert_lock_wait_is_bounded(path, || save(&layout, scope, &changes));
        assert_lock_wait_is_bounded(path, || {
            save_config_checked(&layout, scope, &changes, &toml::Table::new(), |_| Ok(()))
        });
        assert!(
            !path.exists(),
            "failed settings saves must leave disk unchanged"
        );
    }
    let path = history_path(&layout).unwrap();
    assert_lock_wait_is_bounded(&path, || save_history(&layout, &["/drafter".into()], 10));
    assert_lock_wait_is_bounded(&path, || load_history(&layout, 10).map(|_| ()));
    assert!(
        !path.exists(),
        "failed history saves must leave disk unchanged"
    );
    save_config(&layout, Scope::Project, &changes).unwrap();
    save_history(&layout, &["/drafter".into()], 10).unwrap();
    assert_eq!(load_history(&layout, 10).unwrap(), ["/drafter"]);
}

#[cfg(unix)]
#[test]
fn record_locks_interoperate_with_native_flock_and_preserve_contention_errors() {
    use std::os::fd::AsRawFd;

    let (_home, _project, layout) = fixture();
    let path = layout.user_dir.join("native-lock-test.json");
    let native = open_lock(&path).unwrap();
    for mode in [libc::LOCK_SH, libc::LOCK_EX] {
        // SAFETY: native owns a live descriptor; flock takes no pointers.
        assert_eq!(unsafe { libc::flock(native.as_raw_fd(), mode) }, 0);
        let error = try_locked(&path).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::WouldBlock,
            "native lock mode {mode} must exclude a private record writer"
        );
        // SAFETY: native still owns the same live descriptor.
        assert_eq!(unsafe { libc::flock(native.as_raw_fd(), libc::LOCK_UN) }, 0);
    }
    let writer = try_locked(&path).unwrap();
    assert_eq!(
        // SAFETY: native owns a live descriptor; the nonblocking probe cannot hang the test.
        unsafe { libc::flock(native.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().kind(),
        std::io::ErrorKind::WouldBlock
    );
    writer.unlock().unwrap();
    assert!(
        try_locked(&path).is_ok(),
        "explicit unlock releases ownership"
    );
}

#[test]
fn effective_sources_follow_layers_even_when_values_equal_defaults() {
    let (_home, _project, layout) = fixture();
    let global: toml::Table = "[ui]\ntips = true\n".parse().unwrap();
    let project: toml::Table = "[ui]\ntips = true\n".parse().unwrap();
    let empty = toml::Table::new();
    for (global, project, overlay, source) in [
        (&empty, &empty, &empty, "default"),
        (&global, &empty, &empty, "global"),
        (&global, &project, &empty, "project"),
        (&global, &project, &global, "CLI flag"),
    ] {
        let config =
            crate::config::types::Config::from_layers(&layout, global, project, overlay).unwrap();
        assert_eq!(config.setting_sources["ui.tips"], source);
    }
}

#[test]
fn model_review_reports_only_actual_project_removals_and_does_not_save() {
    let (_home, _project, layout) = fixture();
    save_config(
        &layout,
        Scope::Project,
        &[
            Change::Set {
                path: "model".into(),
                value: "old".into(),
            },
            Change::Set {
                path: "provider".into(),
                value: "local".into(),
            },
            Change::Set {
                path: "endpoint.base_url".into(),
                value: "http://127.0.0.1:9999/v1".into(),
            },
            Change::Set {
                path: "model_profile".into(),
                value: "".into(),
            },
            Change::Set {
                path: "context_window".into(),
                value: 4096.into(),
            },
            Change::Set {
                path: "task.prefer_subagents".into(),
                value: true.into(),
            },
        ],
    )
    .unwrap();
    let before = [
        load_layer(&layout, Scope::Global).unwrap(),
        load_layer(&layout, Scope::Project).unwrap(),
    ];
    let config = load_config(&layout, &toml::Table::new()).unwrap();
    let selection = crate::llm::model_config::ModelSelection::new("local", "new");
    let changes = crate::config::selection::changes(&config, &selection, "main").unwrap();
    let review = review_model_config(&layout, Scope::Global, &changes, Some("main")).unwrap();
    let removed: Vec<_> = review
        .effects()
        .iter()
        .filter(|(scope, _, removed)| *scope == Scope::Project && *removed)
        .map(|(_, path, _)| path.as_str())
        .collect();
    assert_eq!(
        removed,
        [
            "context_window",
            "endpoint",
            "model",
            "model_profile",
            "provider"
        ]
    );
    assert_eq!(load_layer(&layout, Scope::Global).unwrap(), before[0]);
    assert_eq!(load_layer(&layout, Scope::Project).unwrap(), before[1]);
    save_reviewed_model_config(&layout, &review).unwrap();
    let project = load_layer(&layout, Scope::Project).unwrap();
    assert!(project.get("model").is_none());
    assert!(project.get("context_window").is_none());
    assert_eq!(project["task"]["prefer_subagents"].as_bool(), Some(true));
    assert_eq!(
        load_config(&layout, &toml::Table::new()).unwrap().model,
        "new"
    );
    let second_review =
        review_model_config(&layout, Scope::Global, &changes, Some("main")).unwrap();
    assert!(
        second_review
            .effects()
            .iter()
            .all(|(scope, _, _)| *scope != Scope::Project),
        "absent project overrides must not be advertised as removals"
    );
}

#[test]
fn stale_model_reviews_refuse_both_layers_without_writes() {
    for changed_scope in [Scope::Global, Scope::Project] {
        let (_home, _project, layout) = fixture();
        let config = load_config(&layout, &toml::Table::new()).unwrap();
        let selection = crate::llm::model_config::ModelSelection::new("local", "reviewed");
        let changes = crate::config::selection::changes(&config, &selection, "main").unwrap();
        let review = review_model_config(&layout, Scope::Global, &changes, Some("main")).unwrap();
        save_config(
            &layout,
            changed_scope,
            &[Change::Set {
                path: "model".into(),
                value: "concurrent".into(),
            }],
        )
        .unwrap();
        let before = layout.config_files().map(|path| std::fs::read(path).ok());
        assert!(
            save_reviewed_model_config(&layout, &review)
                .unwrap_err()
                .to_string()
                .contains("changed since review")
        );
        assert_eq!(
            layout.config_files().map(|path| std::fs::read(path).ok()),
            before
        );
    }
}

#[test]
fn model_review_does_not_advertise_or_publish_unselected_layer_migrations() {
    for (scope, role) in [(Scope::Project, Some("main")), (Scope::Global, None)] {
        let (_home, _project, layout) = fixture();
        let config = load_config(&layout, &toml::Table::new()).unwrap();
        let unselected = usize::from(scope == Scope::Global);
        save(
            &layout,
            if unselected == 0 {
                Scope::Global
            } else {
                Scope::Project
            },
            &[],
        )
        .unwrap();
        let legacy = b"[model_roles.smol]\nprovider='mlx'\nmodel='legacy-small'\n";
        write_private(&layout.config_files()[unselected], legacy).unwrap();
        let changes = if role.is_some() {
            crate::config::selection::changes(
                &config,
                &crate::llm::model_config::ModelSelection::new("local", "new"),
                "main",
            )
            .unwrap()
        } else {
            vec![Change::Remove {
                path: "model_roles.coach".into(),
            }]
        };
        let review = review_model_config(&layout, scope, &changes, role).unwrap();
        assert!(
            review
                .effects()
                .iter()
                .all(|(effect_scope, _, _)| *effect_scope == scope),
            "unpublished migrations must not appear as saved effects"
        );
        save_reviewed_model_config(&layout, &review).unwrap();
        assert_eq!(
            std::fs::read(&layout.config_files()[unselected]).unwrap(),
            legacy
        );
    }
}

#[test]
fn reset_provider_clears_inherited_fields_and_survives_reload() {
    let (_home, _project, layout) = fixture();
    let providers: toml::Table = toml::from_str(
        r#"
[docker-model-runner]
label = "Docker"
base_url = "http://127.0.0.1:12434/engines/v1"
api_key_env = "DOCKER_TEST_KEY"
api_key_envs = ["DOCKER_TEST_KEY_2"]
headers = [{name = "X-Test", value = { literal = "fixture" }}]
"#,
    )
    .unwrap();
    save_config(
        &layout,
        Scope::Global,
        &[Change::Set {
            path: "providers".into(),
            value: providers.into(),
        }],
    )
    .unwrap();
    reset_provider(&layout, Scope::Project, "docker-model-runner").unwrap();
    let loaded = load_config(&layout, &toml::Table::new()).unwrap();
    let reset = &loaded.providers["docker-model-runner"];
    assert!(reset.is_reset());
    assert!(reset.api_key_env.as_deref().is_none_or(str::is_empty));
    assert!(reset.api_key_envs.is_empty());
    assert!(reset.headers.as_ref().is_none_or(Vec::is_empty));
    assert!(
        schema::get(
            &load_layer(&layout, Scope::Global).unwrap(),
            "providers.docker-model-runner.base_url"
        )
        .is_some()
    );
}

#[test]
fn reset_provider_global_clears_project_override_but_keeps_other_providers() {
    let (_home, _project, layout) = fixture();
    for scope in [Scope::Global, Scope::Project] {
        for id in ["mlx-serve", "keep"] {
            save_config(
                &layout,
                scope,
                &[Change::Set {
                    path: format!("providers.{id}.base_url"),
                    value: "http://127.0.0.1:11234/v1".into(),
                }],
            )
            .unwrap();
        }
    }
    reset_provider(&layout, Scope::Global, "mlx-serve").unwrap();
    let loaded = load_config(&layout, &toml::Table::new()).unwrap();
    assert!(loaded.providers["mlx-serve"].is_reset());
    assert!(loaded.providers["keep"].enabled);
    let project = load_layer(&layout, Scope::Project).unwrap();
    assert!(schema::get(&project, "providers.mlx-serve").is_none());
    assert!(schema::get(&project, "providers.keep").is_some());
}

#[test]
fn reset_provider_accepts_local_reset_records_but_not_local_endpoints() {
    let (_home, _project, layout) = fixture();
    for id in ["mlx", "apple"] {
        reset_provider(&layout, Scope::Global, id).unwrap();
        assert!(load_config(&layout, &toml::Table::new()).unwrap().providers[id].is_reset());
        assert!(
            save_config(
                &layout,
                Scope::Global,
                &[Change::Set {
                    path: format!("providers.{id}.base_url"),
                    value: "https://example.test".into()
                }]
            )
            .is_err()
        );
    }
}
