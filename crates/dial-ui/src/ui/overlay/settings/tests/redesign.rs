use super::*;

#[test]
fn changed_filter_spans_categories_and_folded_rows_without_searching_values() {
    let rig = Rig::new();
    let global = table(
        "[ui]\ntips = false\n[memory.hindsight]\nendpoint = 'https://private.example'\n[secrets.values]\ntoken = 'never-show-this-secret'\n",
    );
    let project = table("[ui]\nmotion = false\n");
    let mut o = SettingsOverlay::new(global.clone(), project, global, Scope::Global, vec![]);
    assert!(matches!(
        send(&mut o, &rig.keys, ctrl('o')),
        OverlayEvent::None
    ));
    for path in ["ui.tips", "memory.hindsight.endpoint", "secrets.values"] {
        assert!(shows(&o, path), "missing override {path}");
    }
    assert!(!shows(&o, "ui.motion"));
    o.search = "never-show-this-secret".into();
    assert!(o.visible().is_empty(), "values must not be searchable");
    o.search.clear();
    let shown = draw(&mut o, &rig, 130, 30);
    assert!(!shown.contains("never-show-this-secret"), "{shown}");
    send(&mut o, &rig.keys, ctrl('s'));
    assert!(shows(&o, "ui.motion"));
    assert!(!shows(&o, "ui.tips"));
    send(&mut o, &rig.keys, key(KeyCode::Delete));
    assert!(
        shows(&o, "ui.motion"),
        "pending removal stays until settled"
    );
    o.settled("ui.motion", Err("rejected".into()));
    assert!(shows(&o, "ui.motion"));
    send(&mut o, &rig.keys, key(KeyCode::Delete));
    o.settled("ui.motion", Ok(Value::Boolean(true)));
    assert!(o.visible().is_empty());
    assert!(draw(&mut o, &rig, 130, 30).contains("No saved project overrides"));
    send(&mut o, &rig.keys, key(KeyCode::Esc));
    assert!(!o.changed_only);
}

#[test]
fn source_details_explain_inheritance_fallback_and_runtime_precedence() {
    let rig = Rig::new();
    let mut o = SettingsOverlay::new(
        table("[ui]\ntips = false"),
        Table::new(),
        table("[ui]\ntips = false"),
        Scope::Project,
        vec![],
    );
    assert!(o.focus_path("ui.tips"));
    let def = crate::config::schema::registry()
        .iter()
        .find(|def| def.path == "ui.tips")
        .unwrap();
    assert!(
        o.source_detail(def)
            .contains("Inherited; edit adds a project override")
    );
    crate::config::schema::set(&mut o.project, def.path, Value::Boolean(true));
    assert!(o.source_detail(def).contains("Delete: global (off)"));
    o.scope = Scope::Global;
    assert!(o.source_detail(def).contains("Delete: project (on)"));
    o.sources.insert(def.path.into(), "CLI flag");
    assert!(
        o.source_detail(def)
            .contains("reset keeps this runtime value")
    );
    assert_eq!(
        o.after_unset(def, Scope::Global),
        Some(Value::Boolean(false))
    );
    let shown = draw(&mut o, &rig, 180, 30);
    assert!(
        shown.contains("CLI flag wins over saved settings"),
        "{shown}"
    );
}

#[test]
fn contextual_focus_reaches_advanced_and_inactive_rows_without_changing_values() {
    let mut o = overlay();
    o.scope = Scope::Project;
    o.search = "missing".into();
    o.changed_only = true;
    assert!(!o.focus_path("unknown.setting"));
    assert_eq!(o.search, "missing");
    for path in ["memory.hindsight.endpoint", "ui.theme", "laya.enabled"] {
        assert!(o.focus_path(path));
        assert_eq!(path_at_cursor(&o), path);
        assert_eq!(o.scope, Scope::Project);
        assert!(o.pending.is_empty());
        assert!(!o.changed_only);
    }
}

#[test]
fn browsing_appearance_and_other_categories_keeps_the_header_height_stable() {
    let rig = Rig::new();
    let mut o = overlay();
    for width in [60, 130] {
        for path in [
            "ui.theme",
            "sandbox.mode",
            "ui.show_tools",
            "memory.backend",
        ] {
            assert!(o.focus_path(path));
            let shown = draw(&mut o, &rig, width, 30);
            assert_eq!(
                shown.lines().position(|line| line.contains("Search:")),
                Some(3),
                "header moved for {path}:\n{shown}"
            );
            assert!(
                !shown.contains("Preview: PREVIEW") && !shown.contains("Preview theme:"),
                "browsing must not expand a preview for {path}:\n{shown}"
            );
            assert!(o.pending.is_empty());
        }
    }
}

#[test]
fn context_page_keeps_active_limits_separate_from_saved_defaults() {
    let mut rig = Rig::new();
    rig.status.context_window = 32768;
    rig.status.prompt_tokens = Some(12000);
    rig.status.context_estimated = true;
    rig.status.loaded_context_window = Some(65536);
    rig.status.compaction_reserve = Some(4096);
    let mut o = overlay().context_defaults();
    for path in [
        "local_context_window",
        "context_window",
        "compaction.enabled",
        "harness.max_tokens",
    ] {
        assert!(shows(&o, path), "missing context setting {path}");
    }
    assert!(!shows(&o, "temperature"));
    let shown = draw(&mut o, &rig, 160, 30);
    for expected in [
        "Context & history",
        "active limit 32768",
        "12000 estimated",
        "capacity: 65536 (read-only)",
        "reserve: 4096",
    ] {
        assert!(shown.contains(expected), "{expected}: {shown}");
    }
    o.effective
        .insert("context_window".into(), Value::Integer(8192));
    assert!(draw(&mut o, &rig, 160, 30).contains("active limit 32768"));
    assert!(o.pending.is_empty());
    let unknown = draw(&mut o, &Rig::new(), 160, 30);
    assert!(
        unknown.contains("capacity: unknown (read-only)"),
        "{unknown}"
    );
    for (w, h) in [(40, 10), (20, 4), (1, 1)] {
        let _ = draw(&mut o, &rig, w, h);
    }
}

#[test]
fn memory_landing_explains_backend_models_scope_and_readiness_without_credentials() {
    let rig = Rig::new();
    let values = table(
        "[memory]\nbackend = 'semantic'\n[model_roles.memory]\nprovider = 'local'\nmodel = 'extractor'\n[model_roles.memory.endpoint]\nbase_url = 'http://localhost:1234'\napi_key_env = 'SECRET_REFERENCE'\n[model_roles.embedding]\nprovider = 'local'\nmodel = 'embedder'\n",
    );
    let mut o = SettingsOverlay::new(values.clone(), Table::new(), values, Scope::Global, vec![]);
    assert!(o.focus_path("memory.backend"));
    o.set_hint(
        "memory.backend",
        "Needs setup · local embeddings missing".into(),
    );
    let shown = draw(&mut o, &rig, 180, 30);
    for expected in [
        "backend: semantic",
        "Extraction model: extractor",
        "embedding model: embedder",
        "project facts + explicit user-wide facts",
        "Readiness: Needs setup",
        "choosing a model alone never enables memory",
    ] {
        assert!(shown.contains(expected), "{expected}: {shown}");
    }
    for hidden in ["SECRET_REFERENCE", "http://localhost:1234"] {
        assert!(!shown.contains(hidden), "{shown}");
    }
    assert!(o.pending.is_empty());
    let _ = draw(&mut o, &rig, 32, 8);
}

#[test]
fn memory_model_links_open_the_exact_existing_role_without_mutating_settings() {
    let rig = Rig::new();
    let mut o = overlay();
    assert!(o.focus_path("memory.backend"));
    for (index, role) in [(6, "memory"), (7, "embedding")] {
        o.cursor = o
            .visible()
            .iter()
            .position(|row| matches!(row, Row::Link(i) if *i == index))
            .unwrap();
        assert!(
            matches!(send(&mut o, &rig.keys, key(KeyCode::Enter)), OverlayEvent::Action(action) if action == format!("open:role:{role}"))
        );
        assert!(o.pending.is_empty());
    }
}
