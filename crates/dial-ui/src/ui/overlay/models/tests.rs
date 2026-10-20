use super::super::settings::testkit::*;
use super::*;
use crate::llm::model_config::UiConfig;

fn favorite_routes(selections: Vec<ModelSelection>) -> Vec<FavoriteRoute> {
    selections
        .into_iter()
        .map(|selection| FavoriteRoute {
            effective: selection.clone(),
            selection,
        })
        .collect()
}

fn entry(
    provider: &str,
    model: &str,
    source: &str,
    available: bool,
    context: Option<u32>,
) -> ModelEntry {
    ModelEntry {
        selection: ModelSelection {
            provider: provider.into(),
            model: model.into(),
            base_url: Some(format!("http://{provider}/v1")),
            context_window: context,
            ..ModelSelection::default()
        },
        label: model.into(),
        source: source.into(),
        available,
        metadata: Default::default(),
    }
}

fn provider(id: &str, label: &str, state: &str, models: usize) -> ProviderStatus {
    ProviderStatus {
        id: id.into(),
        label: label.into(),
        base_url: Some(format!("http://{id}/v1")),
        state: state.into(),
        detail: String::new(),
        models,
    }
}

fn discovery() -> Discovery {
    Discovery {
        models: vec![
            entry("ollama", "llama3", "Ollama", true, None),
            entry(
                "lm-studio",
                "qwen-chat",
                "LM Studio · loads on first use",
                true,
                None,
            ),
            entry(
                "lm-studio",
                "qwen-warm",
                "LM Studio · loaded",
                true,
                Some(32768),
            ),
            ModelEntry {
                metadata: crate::llm::catalog::ModelMetadata {
                    kind: ModelKind::Embedding,
                    ..Default::default()
                },
                ..entry("lm-studio", "nomic-embed", "LM Studio", false, Some(2048))
            },
            entry(
                "openai",
                "gpt-cached",
                "OpenAI (cached, unverified)",
                false,
                None,
            ),
        ],
        providers: vec![
            provider("lm-studio", "LM Studio", "reachable", 3),
            provider("ollama", "Ollama", "reachable", 1),
            provider("openai", "OpenAI", "auth-required", 1),
            provider("llama-cpp", "llama.cpp", "unavailable", 0),
        ],
    }
}

fn roles_with_main() -> Vec<RoleStatus> {
    let main = discovery().models[0].selection.clone();
    let name = main.display_name();
    let mut roles = vec![RoleStatus {
        role: "main",
        assigned: Some(main),
        effective: Some(name.clone()),
        source: RoleSource::Main,
    }];
    for &(role, _) in ROLES
        .iter()
        .filter(|(role, _)| !matches!(*role, "main" | "decision" | "embedding"))
    {
        roles.push(RoleStatus {
            role,
            assigned: None,
            effective: Some(name.clone()),
            source: RoleSource::FollowsMain,
        });
    }
    roles
}

fn assign(status: &mut RoleStatus, selection: ModelSelection) {
    status.effective = Some(selection.display_name());
    status.assigned = Some(selection);
    status.source = RoleSource::Assigned;
}

fn overlay() -> ModelOverlay {
    ModelOverlay::new(Some(discovery()), roles_with_main(), Scope::Global)
}

fn draw(o: &mut ModelOverlay, rig: &Rig, w: u16, h: u16) -> String {
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &|ui: &UiConfig, w| status_preview(ui, w),
    };
    screen(&render(w, h, |f| o.draw(f, &ctx)))
}

fn send(o: &mut ModelOverlay, rig: &Rig, e: Event) -> OverlayEvent {
    o.handle(&e, &rig.keys)
}

fn names(o: &ModelOverlay) -> Vec<String> {
    o.rows().iter().map(|(e, _)| e.label.clone()).collect()
}

#[test]
fn judge_role_can_select_laya_once_a_decision_checkpoint_exists() {
    let rig = Rig::new();
    let judge = ROLES.iter().position(|(role, _)| *role == "judge").unwrap();
    let small = ROLES.iter().position(|(role, _)| *role == "small").unwrap();
    let mut o = overlay();
    o.show_roles();
    o.cursor = small;
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(
        o.rows()
            .iter()
            .all(|(entry, _)| entry.selection.provider != "laya")
    );
    send(&mut o, &rig, key(KeyCode::Esc));

    o.cursor = judge;
    send(&mut o, &rig, key(KeyCode::Enter));
    assert_eq!(o.rows()[0].0.selection.provider, "laya");
    // No checkpoint yet: refused with the way to get one.
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(o.message.as_ref().unwrap().0.contains("dial laya install"));

    let mut roles = roles_with_main();
    roles.push(RoleStatus {
        role: "decision",
        assigned: None,
        effective: Some("Laya · /ckpt".into()),
        source: RoleSource::LayaInstalled,
    });
    o.set_roles(roles);
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::AssignModel { role, selection, .. }
            if role == "judge"
                && selection.provider == crate::config::laya::PROVIDER
                && selection.model == crate::config::laya::JUDGE_MODEL
    ));
}

#[test]
fn discovery_refresh_preserves_role_cursor_and_assignment_target() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_roles();
    send(&mut o, &rig, key(KeyCode::Down));
    o.set_discovery(discovery());
    assert_eq!(o.cursor, 1);
    send(&mut o, &rig, key(KeyCode::Enter));
    assert_eq!(o.target_role.as_deref(), Some("small"));
}

#[test]
fn provider_browsing_keeps_role_assignment_and_cancel_destination() {
    let rig = Rig::new();
    for cancel in [false, true] {
        let mut o = overlay();
        o.show_roles();
        send(&mut o, &rig, key(KeyCode::Down));
        send(&mut o, &rig, key(KeyCode::Enter));
        send(&mut o, &rig, key(KeyCode::Tab));
        // All -> Favorites -> Memory -> Get models -> LM Studio.
        for _ in 0..4 {
            send(&mut o, &rig, key(KeyCode::Down));
        }
        assert_eq!(o.group, Group::Provider("lm-studio".into()));
        assert_eq!(o.target_role.as_deref(), Some("small"));
        send(&mut o, &rig, key(KeyCode::Enter));
        if cancel {
            assert!(matches!(
                send(&mut o, &rig, key(KeyCode::Esc)),
                OverlayEvent::None
            ));
        } else {
            assert!(matches!(
                send(&mut o, &rig, key(KeyCode::Enter)),
                OverlayEvent::AssignModel { role, selection, .. }
                    if role == "small" && selection.provider == "lm-studio"
            ));
        }
        assert_eq!(o.group, Group::Roles);
        assert_eq!(o.cursor, 1);
    }
}

#[test]
fn embedding_available_model_offers_only_the_embedding_role() {
    let rig = Rig::new();
    let mut d = discovery();
    d.models[3].available = true;
    let mut o = ModelOverlay::new(Some(d), roles_with_main(), Scope::Global);
    o.type_text("nomic-embed");
    assert_eq!(o.rows().len(), 1);
    assert!(matches!(send(&mut o, &rig, ctrl('a')), OverlayEvent::None));
    assert_eq!(o.chooser.as_ref().unwrap().roles, ["embedding"]);
    send(&mut o, &rig, key(KeyCode::Down));
    assert!(matches!(send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::AssignModel { role, .. } if role == "embedding"));
    assert!(o.chooser.is_none());
}

#[test]
fn enter_routes_embeddings_to_the_embedding_role_not_main() {
    let rig = Rig::new();
    let mut d = discovery();
    d.models[3].available = true;
    let mut o = ModelOverlay::new(Some(d), roles_with_main(), Scope::Global);
    o.type_text("nomic-embed");
    assert!(
        o.footer(&rig.keys)
            .iter()
            .any(|hint| hint.contains("set embedding"))
    );
    assert!(matches!(send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::AssignModel { role, .. } if role == "embedding"));
}

#[test]
fn embedding_role_picker_offers_unclassified_models_but_not_classified_chat() {
    let rig = Rig::new();
    let mut d = discovery();
    d.models[3].available = true;
    d.models[1].metadata.kind = ModelKind::Chat;
    let mut o = ModelOverlay::new(Some(d.clone()), roles_with_main(), Scope::Global);
    o.show_roles();
    o.cursor = ROLES
        .iter()
        .position(|(role, _)| *role == "embedding")
        .unwrap();
    send(&mut o, &rig, key(KeyCode::Enter));
    assert_eq!(o.target_role.as_deref(), Some("embedding"));
    let listed = names(&o);
    assert_eq!(listed.first().map(String::as_str), Some("nomic-embed"));
    assert!(listed.iter().any(|name| name == "llama3"), "{listed:?}");
    assert!(!listed.iter().any(|name| name == "qwen-chat"), "{listed:?}");
    let shown = draw(&mut o, &rig, 130, 30);
    assert!(shown.contains("unclassified"), "{shown}");
    o.cursor = listed.iter().position(|name| name == "llama3").unwrap();
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::AssignModel { role, selection, .. }
            if role == "embedding" && selection.model == "llama3"
    ));
    // The per-model chooser keeps the strict rule: an unclassified model is not an embedding.
    assert!(!role_accepts("embedding", &d.models[0]));
    let candidates = embedding_candidates(&d);
    assert_eq!(candidates[0].0.label, "nomic-embed");
    assert!(candidates[0].1);
    assert!(
        candidates
            .iter()
            .any(|(e, classified)| e.label == "llama3" && !classified)
    );
    assert!(
        candidates
            .iter()
            .all(|(e, _)| e.label != "qwen-chat" && e.available)
    );
}

#[test]
fn memory_role_row_is_not_set_then_required_or_optional_by_backend() {
    let rig = Rig::new();
    let config = crate::config::Config::default();
    let mut o = ModelOverlay::new(Some(discovery()), config.role_statuses(), Scope::Global);
    o.show_roles();
    let memory = config
        .role_statuses()
        .into_iter()
        .find(|status| status.role == "memory")
        .unwrap();
    assert_eq!(
        memory.source,
        RoleSource::NotSet,
        "memory never follows main"
    );
    assert!(memory.effective.is_none());
    assert!(draw(&mut o, &rig, 140, 30).contains("not set"));
    o.set_memory_required(true);
    assert!(draw(&mut o, &rig, 140, 30).contains("required: assign a model"));
    o.set_memory_required(false);
    o.set_memory_optional(true);
    assert!(draw(&mut o, &rig, 140, 30).contains("optional: assign to save facts"));
}

#[test]
fn non_chat_models_are_visible_but_cannot_be_assigned_to_chat_roles() {
    let rig = Rig::new();
    for kind in [ModelKind::Rerank, ModelKind::Image, ModelKind::Audio] {
        let mut model = entry("custom", "specialized", "provider metadata", true, None);
        model.metadata.kind = kind;
        assert!(!KindFilter::Chat.accepts(&model));
        assert!(KindFilter::Other.accepts(&model));
        assert!(!ROLES.iter().any(|(role, _)| role_accepts(role, &model)));
        let mut o = ModelOverlay::new(
            Some(Discovery {
                models: vec![model],
                providers: vec![],
            }),
            roles_with_main(),
            Scope::Global,
        );
        assert_eq!(names(&o), ["specialized"]);
        assert!(matches!(
            send(&mut o, &rig, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        assert!(o.message.is_some());
    }
}

#[test]
fn display_prose_does_not_determine_a_models_purpose() {
    let model = entry(
        "custom",
        "chat",
        "embedding examples in the documentation",
        true,
        None,
    );
    assert!(!is_embedding(&model));
    assert!(role_accepts("main", &model));
    assert!(!role_accepts("embedding", &model));
}

#[test]
fn roles_navigation_clears_hidden_search_and_allows_clear() {
    let rig = Rig::new();
    let mut o = overlay();
    assign(&mut o.roles[1], discovery().models[2].selection.clone());
    send(&mut o, &rig, ch('q'));
    send(&mut o, &rig, key(KeyCode::Tab));
    send(&mut o, &rig, key(KeyCode::Up));
    assert_eq!(o.group, Group::Roles);
    assert!(o.search.is_empty());
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    send(&mut o, &rig, key(KeyCode::Down));
    assert!(
        matches!(send(&mut o, &rig, ch('x')), OverlayEvent::ClearRole { role, .. } if role == "small")
    );
    assert_eq!(o.group, Group::Roles);
}

#[test]
fn task_role_assignment_and_clear_use_registry_row_and_selected_scope() {
    let rig = Rig::new();
    let mut o = overlay();
    o.scope = Scope::Project;
    o.show_roles();
    let index = ROLES.iter().position(|(role, _)| *role == "task").unwrap();
    o.cursor = index;
    send(&mut o, &rig, key(KeyCode::Enter));
    assert_eq!(o.target_role.as_deref(), Some("task"));
    let OverlayEvent::AssignModel {
        role,
        selection,
        scope,
    } = send(&mut o, &rig, key(KeyCode::Enter))
    else {
        panic!("expected task assignment");
    };
    assert_eq!(role, "task");
    assert_eq!(scope, Scope::Project);
    assert_eq!(o.cursor, index);
    let mut roles = roles_with_main();
    assign(
        roles.iter_mut().find(|row| row.role == "task").unwrap(),
        selection,
    );
    o.set_roles(roles);
    assert!(matches!(
        send(&mut o, &rig, ch('x')),
        OverlayEvent::ClearRole { role, scope: Scope::Project } if role == "task"
    ));
}

#[test]
fn cleared_role_refresh_removes_assignment_and_preserves_selected_row() {
    let rig = Rig::new();
    for (index, &(role, _)) in ROLES.iter().enumerate().skip(1) {
        let mut config = crate::config::types::Config {
            model: "main-model".into(),
            ..Default::default()
        };
        config.codebase.architecture_model.clear();
        let provider = if role == "decision" { "laya" } else { "mlx" };
        config
            .model_roles
            .insert(role.into(), ModelSelection::new(provider, "assigned-model"));
        let mut o = ModelOverlay::new(None, config.role_statuses(), Scope::Project);
        o.show_roles();
        o.cursor = index;
        assert!(draw(&mut o, &rig, 130, 30).contains("assigned-model"));
        assert!(matches!(
            send(&mut o, &rig, ch('x')),
            OverlayEvent::ClearRole { role: cleared, scope: Scope::Project } if cleared == role
        ));
        // A failed save must leave the original model visible. The controller alone
        // supplies the successfully saved state.
        assert!(o.role_selection(role).is_some());
        config.model_roles.remove(role);
        o.set_roles(config.role_statuses());
        assert!(o.role_selection(role).is_none());
        assert_eq!(o.cursor, index);
        assert_eq!(o.group, Group::Roles);
        let shown = draw(&mut o, &rig, 130, 30);
        assert!(!shown.contains("assigned-model"), "{role}: {shown}");
        assert!(o.role_selection("main").is_some());
    }
}

#[test]
fn role_picker_resets_incompatible_filters_and_returns_to_selected_role() {
    let rig = Rig::new();
    let mut o = overlay();
    send(&mut o, &rig, key(KeyCode::Right));
    send(&mut o, &rig, key(KeyCode::Right));
    assert_eq!(o.kind, KindFilter::Embedding);
    send(&mut o, &rig, ch('n'));
    send(&mut o, &rig, key(KeyCode::Tab));
    send(&mut o, &rig, key(KeyCode::Up));
    send(&mut o, &rig, key(KeyCode::Enter));
    send(&mut o, &rig, key(KeyCode::Down));
    send(&mut o, &rig, key(KeyCode::Enter));
    assert_eq!(o.kind, KindFilter::All);
    assert!(o.search.is_empty());
    assert!(!o.rows().is_empty());
    assert!(o.rows().iter().all(|(entry, _)| !is_embedding(entry)));
    send(&mut o, &rig, key(KeyCode::Esc));
    assert_eq!(o.cursor, 1);
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(
        matches!(send(&mut o, &rig, key(KeyCode::Enter)), OverlayEvent::AssignModel { role, .. } if role == "small")
    );
    assert_eq!(o.group, Group::Roles);
    assert_eq!(o.cursor, 1);
}

#[test]
fn show_roles_moves_focus_to_list_and_clears_search() {
    let rig = Rig::new();
    let mut o = overlay();
    send(&mut o, &rig, ch('q'));
    send(&mut o, &rig, key(KeyCode::Tab));
    o.show_roles();
    assert_eq!(o.focus, Focus::List);
    assert!(o.search.is_empty());
    send(&mut o, &rig, key(KeyCode::Down));
    assert_eq!(o.cursor, 1);
    send(&mut o, &rig, key(KeyCode::Enter));
    assert_eq!(o.target_role.as_deref(), Some("small"));
}

#[test]
fn empty_catalog_still_allows_roles() {
    let rig = Rig::new();
    let mut o = ModelOverlay::new(Some(Discovery::default()), roles_with_main(), Scope::Global);
    send(&mut o, &rig, key(KeyCode::Tab));
    send(&mut o, &rig, key(KeyCode::Up));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert_eq!(o.focus, Focus::List);
}

#[test]
fn checkpoint_paste_updates_prompt_not_model_search() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_roles();
    o.open_prompt("decision");
    send(&mut o, &rig, Event::Paste("/tmp/checkpoint".into()));
    assert_eq!(o.prompt.as_ref().unwrap().text, "/tmp/checkpoint");
    assert!(o.search.is_empty());
    assert_eq!(o.group, Group::Roles);
    assert!(
        matches!(send(&mut o, &rig, key(KeyCode::Enter)), OverlayEvent::AssignModel { role, selection, .. } if role == "decision" && selection.model == "/tmp/checkpoint")
    );
}

#[test]
fn role_chooser_keeps_header_shortcut_from_opening_a_second_modal() {
    let rig = Rig::new();
    let mut o = overlay();
    send(&mut o, &rig, ctrl('a'));
    send(&mut o, &rig, key(KeyCode::F(8)));
    assert!(o.chooser.is_some());
    assert!(o.headers_editor.is_none());
    send(&mut o, &rig, key(KeyCode::Down));
    assert!(
        matches!(send(&mut o, &rig, key(KeyCode::Enter)), OverlayEvent::AssignModel { role, .. } if role == "small")
    );
}

#[test]
fn header_editing_needs_a_role_and_only_ever_assigns_that_role() {
    let rig = Rig::new();
    let mut o = overlay();
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::F(8))),
        OverlayEvent::None
    ));
    assert!(
        o.headers_editor.is_none(),
        "browsing alone must not edit main"
    );
    assert!(
        o.message
            .as_ref()
            .is_some_and(|(text, _)| text.contains("Choose a role")),
    );
    o.show_roles();
    o.cursor = ROLES.iter().position(|(role, _)| *role == "small").unwrap();
    send(&mut o, &rig, key(KeyCode::Enter));
    send(&mut o, &rig, key(KeyCode::F(8)));
    assert!(o.headers_editor.is_some());
    let OverlayEvent::AssignModel { role, .. } = send(&mut o, &rig, key(KeyCode::F(10))) else {
        panic!("saving headers assigns the role being edited");
    };
    assert_eq!(role, "small");
}

#[test]
fn narrow_panes_do_not_focus_the_hidden_groups_and_the_legend_names_favorites() {
    let rig = Rig::new();
    let mut o = overlay();
    let narrow = draw(&mut o, &rig, 50, 24);
    assert!(!narrow.contains("Shift+Tab groups"), "{narrow}");
    send(&mut o, &rig, key(KeyCode::BackTab));
    assert_eq!(o.focus, Focus::List, "hidden groups cannot take focus");
    let wide = draw(&mut o, &rig, 100, 30);
    assert!(wide.contains("* favorite"), "{wide}");
    assert!(wide.contains("Ctrl+F favorite"), "{wide}");
    assert!(wide.contains("Shift+Tab groups"), "{wide}");
    send(&mut o, &rig, key(KeyCode::BackTab));
    assert_eq!(o.focus, Focus::Groups);
}

#[test]
fn all_models_keeps_main_first_and_pins_favorites_above_other_models() {
    let mut o = overlay();
    let main = o.highlighted().unwrap().selection.clone();
    let favorite = o.rows().last().unwrap().0.selection.clone();
    assert!(!same_model(&main, &favorite));
    o.set_favorites(favorite_routes(vec![favorite.clone()]));
    let rows = o.rows();
    assert!(same_model(&rows[0].0.selection, &main));
    assert!(same_model(&rows[1].0.selection, &favorite));
}

#[test]
fn favorites_toggle_waits_for_save_and_removal_clamps_the_list() {
    let rig = Rig::new();
    let mut o = overlay();
    o.scope = Scope::Project;
    let selection = o.highlighted().unwrap().selection.clone();
    let event = send(
        &mut o,
        &rig,
        Event::Key(KeyEvent::new(
            KeyCode::Char('f'),
            crossterm::event::KeyModifiers::CONTROL,
        )),
    );
    let OverlayEvent::Set { path, value, scope } = event else {
        panic!("expected save")
    };
    assert_eq!(path, "model_favorites");
    assert_eq!(scope, Scope::Project);
    assert!(
        !o.is_favorite(&selection),
        "failed saves must not update favorites"
    );
    let favorites: Vec<ModelSelection> = value.try_into().unwrap();
    assert_eq!(favorites, vec![selection.clone()]);
    o.set_favorites(favorite_routes(favorites));
    o.group = Group::Favorites;
    assert_eq!(o.rows().len(), 1);
    let OverlayEvent::Set { value, .. } = o.toggle_favorite() else {
        panic!("expected removal")
    };
    o.set_favorites(favorite_routes(value.try_into().unwrap()));
    assert!(o.rows().is_empty());
    assert_eq!(o.cursor, 0);
    assert!(matches!(o.toggle_favorite(), OverlayEvent::None));
}

#[test]
fn favorites_identity_filters_and_rendering_survive_discovery() {
    let rig = Rig::new();
    let mut o = overlay();
    let selection = o.discovery.as_ref().unwrap().models[1].selection.clone();
    let mut other_endpoint = selection.clone();
    other_endpoint.base_url = Some("http://other-endpoint".into());
    o.set_favorites(favorite_routes(vec![other_endpoint]));
    o.group = Group::Favorites;
    assert!(o.rows().is_empty());
    let mut favorite = selection.clone();
    favorite.context_window = Some(123);
    o.set_favorites(favorite_routes(vec![favorite]));
    assert_eq!(o.rows().len(), 1, "context is metadata, not identity");
    o.kind = KindFilter::Embedding;
    assert!(o.rows().is_empty());
    o.kind = KindFilter::All;
    o.search = "no-match-at-all".into();
    assert!(o.rows().is_empty());
    o.search.clear();
    o.set_discovery(discovery());
    assert_eq!(o.rows().len(), 1);
    let shown = draw(&mut o, &rig, 120, 24);
    assert!(shown.contains("Favorites"), "{shown}");
    assert!(
        shown
            .lines()
            .any(|line| line.contains(&selection.model) && line.contains('*')),
        "{shown}"
    );
    o.focus = Focus::Groups;
    assert!(matches!(o.toggle_favorite(), OverlayEvent::None));
}

#[test]
fn capability_icons_labels_and_search_use_metadata_in_every_glyph_preset() {
    use crate::ui::theme::Glyphs;
    for glyphs in [Glyphs::Unicode, Glyphs::Nerd, Glyphs::Ascii] {
        let mut rig = Rig::new();
        rig.theme = rig.theme.with_glyphs(glyphs);
        let mut model = entry("mlx", "qwen-local", "Downloaded", true, Some(32768));
        model.metadata.vision = Some(true);
        model.metadata.tools = Some(true);
        model.metadata.reasoning = Some(true);
        let mut o = ModelOverlay::new(
            Some(Discovery {
                models: vec![model.clone()],
                providers: vec![],
            }),
            vec![],
            Scope::Global,
        );
        let shown = draw(&mut o, &rig, 150, 24);
        let row = shown
            .lines()
            .find(|line| line.contains("mlx/qwen-local"))
            .unwrap();
        for icon in [Sym::IconVision, Sym::IconTools, Sym::IconReasoning] {
            assert!(row.contains(rig.theme.sym(icon)), "{shown}");
        }
        let mut roles = roles_with_main();
        assign(
            roles.iter_mut().find(|r| r.role == "main").unwrap(),
            model.selection.clone(),
        );
        o.set_roles(roles);
        for selected in [false, true] {
            let line = o.model_line(&rig.theme, &model, &[], selected, 100);
            let icons = line.spans.last().unwrap();
            assert_eq!(icons.content, capability_cell(&rig.theme, &model, 8));
            // On the selection fill every detail takes the body color.
            let tok = if selected { Tok::Text } else { Tok::Muted };
            assert_eq!(icons.style.fg, rig.theme.fg(tok).fg);
            assert_eq!(line.width(), 100);
            assert_eq!(
                line.spans[..line.spans.len() - 1]
                    .iter()
                    .map(|span| span.width())
                    .sum::<usize>(),
                92
            );
        }
        let role = o.role_line(&rig.theme, 0, 100);
        assert_eq!(
            role.spans.last().unwrap().content,
            capability_cell(&rig.theme, &model, 8)
        );
        // Row 0 is under the cursor, so it reads in the body color on the selection fill.
        assert_eq!(
            role.spans.last().unwrap().style.fg,
            rig.theme.fg(Tok::Text).fg
        );
        assert!(shown.contains("vision") && shown.contains("tools") && shown.contains("reasoning"));
        o.type_text("vision");
        assert_eq!(names(&o), ["qwen-local"]);
        model.metadata.tools = Some(false);
        model.metadata.reasoning = None;
        let detail = capability_detail(&rig.theme, &model);
        assert!(detail.contains("tools no") && detail.contains("reasoning unknown"));
        assert!(!capability_badges(&rig.theme, &model).contains(rig.theme.sym(Sym::IconTools)));
        let mut embed = entry("ollama", "embed-model", "Ollama", false, None);
        embed.metadata.kind = ModelKind::Embedding;
        assert_eq!(
            capability_badges(&rig.theme, &embed),
            rig.theme.sym(Sym::IconEmbedding)
        );
        for w in [28, 40, 60, 80, 100] {
            assert!(o.model_line(&rig.theme, &model, &[], true, w).width() <= w);
        }
    }
}

#[test]
fn rows_and_details_show_lora_compatibility_and_the_main_adapter() {
    let rig = Rig::new();
    let local = |label: &str, lora| ModelEntry {
        selection: ModelSelection {
            model: format!("/models/{label}"),
            ..ModelSelection::default()
        },
        label: label.into(),
        source: "LM Studio download (MLX)".into(),
        available: true,
        metadata: crate::llm::catalog::ModelMetadata {
            lora,
            ..Default::default()
        },
    };
    let models = vec![
        local("qwen-mlx", Some(true)),
        local("gemma-mlx", Some(false)),
        entry("lm-studio", "qwen-served", "LM Studio · loaded", true, None),
    ];
    let main = models[0].selection.clone();
    let mut roles = roles_with_main();
    assign(roles.iter_mut().find(|r| r.role == "main").unwrap(), main);
    let mut o = ModelOverlay::new(
        Some(Discovery {
            models,
            providers: vec![],
        }),
        roles,
        Scope::Global,
    )
    .with_main_lora(Lora::Active(4))
    .with_learning(true);
    let shown = draw(&mut o, &rig, 130, 30);
    let row = |name: &str| {
        shown
            .lines()
            .find(|l| l.contains(name))
            .unwrap_or_else(|| panic!("{name} missing:\n{shown}"))
            .to_owned()
    };
    assert!(
        row("mlx/qwen-mlx").contains("accepted v4"),
        "the main model shows its admitted adapter"
    );
    assert!(row("mlx/gemma-mlx").contains("unsupported"));
    assert!(
        row("lm-studio/qwen-served").contains(" - "),
        "a served model has no LoRA column value"
    );
    assert!(
        shown.contains("LoRA v4 accepted; loading not confirmed"),
        "{shown}"
    );
    o.main_lora = Lora::Unsupported;
    let rejected = draw(&mut o, &rig, 130, 30);
    assert!(
        rejected
            .lines()
            .find(|line| line.contains("mlx/qwen-mlx"))
            .unwrap()
            .contains("unsupported")
    );
    assert!(rejected.contains("LoRA unavailable in Dial's current trainer"));
    o.main_lora = Lora::Unknown;
    let unchecked = draw(&mut o, &rig, 130, 30);
    assert!(
        unchecked
            .lines()
            .find(|line| line.contains("mlx/qwen-mlx"))
            .unwrap()
            .contains("unverified")
    );
    assert!(unchecked.contains("training targets have not been verified"));
    // Loaded models rank above merely available ones, so the served model is the second row.
    send(&mut o, &rig, key(KeyCode::Down));
    let shown = draw(&mut o, &rig, 130, 30);
    assert!(shown.contains("no LoRA: served by lm-studio"), "{shown}");
}

#[test]
fn lora_verification_uses_highlighted_local_model_without_assigning_it() {
    let rig = Rig::new();
    let selection = ModelSelection {
        model: "/models/local-test".into(),
        ..Default::default()
    };
    let model = ModelEntry {
        selection: selection.clone(),
        label: "local-test".into(),
        source: "local MLX".into(),
        available: true,
        metadata: crate::llm::catalog::ModelMetadata {
            lora: Some(true),
            ..Default::default()
        },
    };
    let mut o = ModelOverlay::new(
        Some(Discovery {
            models: vec![model.clone()],
            providers: vec![],
        }),
        vec![],
        Scope::Global,
    );
    o.learning = true;
    o.group = Group::All;
    o.focus = Focus::List;
    assert!(
        matches!(send(&mut o, &rig, key(KeyCode::F(6))), OverlayEvent::VerifyLora(found) if found.model == selection.model)
    );
    let mut verified = model;
    verified.metadata.lora_verified = true;
    assert_eq!(lora_cell(&verified, None).0, "verified");
    verified.metadata.lora = Some(false);
    assert_eq!(lora_cell(&verified, None).0, "unsupported");
    o.discovery.as_mut().unwrap().models[0] = entry("lm-studio", "remote", "loaded", true, None);
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::F(6))),
        OverlayEvent::None
    ));
}

#[test]
fn left_column_shows_roles_all_models_and_providers_with_counts_and_state_glyphs() {
    let rig = Rig::new();
    let mut o = overlay();
    let shown = draw(&mut o, &rig, 110, 30);
    for needle in [
        "MODELS",
        "scope: global",
        "Roles",
        "All models",
        "LM Studio",
        "Ollama",
        "OpenAI",
    ] {
        assert!(shown.contains(needle), "missing {needle:?}\n{shown}");
    }
    let line = |n: &str| shown.lines().find(|l| l.contains(n)).unwrap().to_owned();
    assert!(
        line("All models")
            .trim_end_matches('│')
            .trim_end()
            .ends_with('5')
            || line("All models").contains(" 5"),
        "{}",
        line("All models")
    );
    assert!(
        line("LM Studio").contains('●') && line("LM Studio").contains('3'),
        "{}",
        line("LM Studio")
    );
    assert!(
        line("OpenAI").contains('⚠'),
        "auth-required warns: {}",
        line("OpenAI")
    );
    assert!(!shown.contains("llama.cpp"), "empty providers are hidden");
}

#[test]
fn empty_providers_are_not_navigable_and_refresh_leaves_an_empty_group() {
    let mut o = overlay();
    assert!(!o.groups().contains(&Group::Provider("llama-cpp".into())));
    o.group = Group::Provider("ollama".into());
    let mut d = discovery();
    d.models.retain(|m| m.selection.provider != "ollama");
    d.providers
        .iter_mut()
        .find(|p| p.id == "ollama")
        .unwrap()
        .models = 0;
    o.set_discovery(d);
    assert_eq!(o.group, Group::All);
    assert!(!o.groups().contains(&Group::Provider("ollama".into())));
    assert!(o.groups().contains(&Group::Provider("lm-studio".into())));
}

#[test]
fn right_pane_orders_main_first_then_loaded_then_available_then_unavailable() {
    let o = overlay();
    assert_eq!(
        names(&o),
        [
            "llama3",
            "qwen-warm",
            "qwen-chat",
            "nomic-embed",
            "gpt-cached"
        ]
    );
    let rig = Rig::new();
    let mut o = overlay();
    let shown = draw(&mut o, &rig, 120, 30);
    let row = shown
        .lines()
        .find(|l| l.contains("lm-studio/qwen-warm"))
        .unwrap();
    assert!(row.contains("32k") && row.contains("loaded"), "{row}");
    let row = shown
        .lines()
        .find(|l| l.contains("lm-studio/qwen-chat"))
        .unwrap();
    assert!(
        row.contains("loads on first use") && row.contains(" - "),
        "unknown context is '-': {row}"
    );
    let row = shown.lines().find(|l| l.contains("nomic-embed")).unwrap();
    assert!(
        row.contains("unavailable"),
        "availability stays distinct from model purpose: {row}"
    );
    assert!(
        shown
            .lines()
            .find(|l| l.contains("ollama/llama3"))
            .unwrap()
            .contains('●'),
        "main model is marked"
    );
}

#[test]
fn detail_lines_name_context_state_and_roles_using_the_model() {
    let rig = Rig::new();
    let mut roles = roles_with_main();
    assign(
        roles.iter_mut().find(|row| row.role == "coach").unwrap(),
        discovery().models[0].selection.clone(),
    );
    let mut o = ModelOverlay::new(Some(discovery()), roles, Scope::Global);
    let shown = draw(&mut o, &rig, 130, 30);
    assert!(shown.contains("used by: main, coach"), "{shown}");
    send(&mut o, &rig, key(KeyCode::Down));
    let shown = draw(&mut o, &rig, 130, 30);
    assert!(
        shown.contains("context 32k")
            && shown.contains("not used by any role")
            && !shown.contains("Source"),
        "{shown}"
    );
}

#[test]
fn typing_filters_fuzzily_and_highlights_matches() {
    let rig = Rig::new();
    let mut o = overlay();
    for e in typed("qw") {
        send(&mut o, &rig, e);
    }
    assert_eq!(names(&o), ["qwen-warm", "qwen-chat"]);
    let buf = render(120, 30, |f| {
        let c = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &|u: &UiConfig, w| status_preview(u, w),
        };
        o.draw(f, &c)
    });
    let plain = style_of(&buf, "lm-studio/").unwrap();
    let matched = style_of(&buf, "qwen-warm").unwrap();
    assert_ne!(
        plain,
        matched,
        "matched characters are emphasized: {plain:?} vs {matched:?}\n{}",
        screen(&buf)
    );
    assert!(screen(&buf).contains(&format!("{} qw", rig.theme.sym(Sym::Prompt))));
    send(&mut o, &rig, key(KeyCode::Backspace));
    send(&mut o, &rig, key(KeyCode::Backspace));
    assert_eq!(names(&o).len(), 5);
    for e in typed("zzzz") {
        send(&mut o, &rig, e);
    }
    assert!(draw(&mut o, &rig, 120, 30).contains("No models match \"zzzz\""));
    assert!(
        matches!(send(&mut o, &rig, key(KeyCode::Esc)), OverlayEvent::None),
        "first Esc clears the search"
    );
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::Close
    ));
}

#[test]
fn kind_filter_separates_chat_and_embedding_models() {
    let rig = Rig::new();
    let mut o = overlay();
    send(&mut o, &rig, key(KeyCode::Right));
    assert_eq!(o.kind, KindFilter::Chat);
    assert_eq!(
        names(&o),
        ["llama3", "qwen-warm", "qwen-chat", "gpt-cached"]
    );
    send(&mut o, &rig, key(KeyCode::Right));
    assert_eq!(names(&o), ["nomic-embed"]);
    send(&mut o, &rig, key(KeyCode::Right));
    assert_eq!(o.kind, KindFilter::Other);
    assert!(names(&o).is_empty());
    send(&mut o, &rig, key(KeyCode::Right));
    assert_eq!(o.kind, KindFilter::Other, "clamps at the last kind");
    send(&mut o, &rig, key(KeyCode::Left));
    send(&mut o, &rig, key(KeyCode::Left));
    send(&mut o, &rig, key(KeyCode::Left));
    assert_eq!(o.kind, KindFilter::All);
    send(&mut o, &rig, key(KeyCode::Left));
    assert_eq!(
        o.focus,
        Focus::Groups,
        "Left from the first kind goes to the providers"
    );
}

#[test]
fn provider_group_filters_and_groups_are_walked_with_the_keymap() {
    let rig = Rig::new();
    let mut o = overlay();
    send(&mut o, &rig, key(KeyCode::Tab));
    assert_eq!(o.focus, Focus::Groups);
    send(&mut o, &rig, key(KeyCode::Down));
    assert_eq!(o.group, Group::Favorites);
    send(&mut o, &rig, key(KeyCode::Down));
    assert_eq!(o.group, Group::Memory);
    send(&mut o, &rig, key(KeyCode::Down));
    assert_eq!(o.group, Group::Download);
    assert!(draw(&mut o, &rig, 120, 30).contains("Get models"));
    send(&mut o, &rig, key(KeyCode::Down));
    assert_eq!(o.group, Group::Provider("lm-studio".into()));
    assert_eq!(names(&o), ["qwen-warm", "qwen-chat", "nomic-embed"]);
    send(&mut o, &rig, key(KeyCode::Down));
    assert_eq!(names(&o), ["llama3"]);
    send(&mut o, &rig, key(KeyCode::Up));
    send(&mut o, &rig, key(KeyCode::Up));
    assert_eq!(o.group, Group::Download);
    send(&mut o, &rig, key(KeyCode::Up));
    assert_eq!(o.group, Group::Memory);
    send(&mut o, &rig, key(KeyCode::Up));
    assert_eq!(o.group, Group::Favorites);
    send(&mut o, &rig, key(KeyCode::Up));
    assert_eq!(o.group, Group::All);
    send(&mut o, &rig, key(KeyCode::Up));
    assert_eq!(o.group, Group::Roles);
    send(&mut o, &rig, key(KeyCode::Up));
    assert_eq!(o.group, Group::Roles, "stops at the top");
}

#[test]
fn enter_assigns_main_with_the_active_scope_and_ctrl_s_changes_it() {
    let rig = Rig::new();
    let mut o = overlay();
    send(&mut o, &rig, key(KeyCode::Down));
    match send(&mut o, &rig, key(KeyCode::Enter)) {
        OverlayEvent::AssignModel {
            role,
            selection,
            scope,
        } => {
            assert_eq!(
                (role.as_str(), selection.model.as_str(), scope),
                ("main", "qwen-warm", Scope::Global)
            );
            assert_eq!(selection.context_window, Some(32768));
        }
        other => panic!("{other:?}"),
    }
    send(&mut o, &rig, ctrl('s'));
    assert!(draw(&mut o, &rig, 120, 30).contains("scope: project"));
    match send(&mut o, &rig, key(KeyCode::Enter)) {
        OverlayEvent::AssignModel { scope, .. } => assert_eq!(scope, Scope::Project),
        other => panic!("{other:?}"),
    }
}

#[test]
fn unavailable_models_refuse_with_a_visible_reason() {
    let rig = Rig::new();
    let mut o = overlay();
    for _ in 0..3 {
        send(&mut o, &rig, key(KeyCode::Down));
    }
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    let shown = draw(&mut o, &rig, 130, 30);
    assert!(
        shown.contains("Cannot select lm-studio/nomic-embed")
            && shown.contains("provider reports it as unavailable"),
        "{shown}"
    );
    let style = style_of(
        &render(130, 30, |f| {
            let c = DrawCtx {
                theme: &rig.theme,
                keys: &rig.keys,
                status: &rig.status,
                preview_status: &|u: &UiConfig, w| status_preview(u, w),
            };
            o.draw(f, &c)
        }),
        "Cannot select",
    )
    .unwrap();
    assert_eq!(style.fg, rig.theme.fg(Tok::Error).fg);
    // The message goes away on the next key, and unverified cached rows say so.
    send(&mut o, &rig, key(KeyCode::Down));
    assert!(!draw(&mut o, &rig, 130, 30).contains("Cannot select"));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(
        o.message
            .as_ref()
            .unwrap()
            .0
            .contains("provider is not reachable")
    );
    assert!(matches!(send(&mut o, &rig, ctrl('a')), OverlayEvent::None));
    assert!(
        o.chooser.is_none(),
        "the role chooser refuses unavailable models too"
    );
}

#[test]
fn ctrl_a_opens_the_role_chooser_and_assigns_that_role() {
    let rig = Rig::new();
    let mut o = overlay();
    send(&mut o, &rig, key(KeyCode::Down));
    send(&mut o, &rig, ctrl('a'));
    let shown = draw(&mut o, &rig, 120, 30);
    assert!(
        shown.contains("ASSIGN LM-STUDIO/QWEN-WARM TO:")
            && shown.contains("small")
            && shown.contains("planner"),
        "{shown}"
    );
    send(&mut o, &rig, key(KeyCode::Down));
    match send(&mut o, &rig, key(KeyCode::Enter)) {
        OverlayEvent::AssignModel {
            role, selection, ..
        } => assert_eq!(
            (role.as_str(), selection.model.as_str()),
            ("small", "qwen-warm")
        ),
        other => panic!("{other:?}"),
    }
    assert!(o.chooser.is_none());
    send(&mut o, &rig, ctrl('a'));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert!(
        o.chooser.is_none(),
        "Esc cancels the chooser, not the panel"
    );
}

#[test]
fn roles_view_lists_roles_picks_models_and_clears() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_roles();
    let shown = draw(&mut o, &rig, 120, 30);
    assert!(
        shown.contains("main") && shown.contains("ollama/llama3") && shown.contains("follows main"),
        "{shown}"
    );
    assert!(
        shown.contains("coach") && shown.contains("planner"),
        "every role must be listed: {shown}"
    );
    send(&mut o, &rig, key(KeyCode::Down));
    // Pick a model for `small`.
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    let shown = draw(&mut o, &rig, 130, 30);
    assert!(
        shown.contains("choose the model for role \"small\""),
        "{shown}"
    );
    send(&mut o, &rig, key(KeyCode::Down));
    match send(&mut o, &rig, key(KeyCode::Enter)) {
        OverlayEvent::AssignModel {
            role, selection, ..
        } => assert_eq!(
            (role.as_str(), selection.model.as_str()),
            ("small", "qwen-warm")
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(o.group, Group::Roles, "back on the roles list");
    // Esc while picking returns to the roles list instead of closing.
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert_eq!(o.group, Group::Roles);
    // The controller reports the new assignment.
    let mut roles = roles_with_main();
    assign(&mut roles[1], discovery().models[2].selection.clone());
    o.set_roles(roles);
    assert!(draw(&mut o, &rig, 120, 30).contains("lm-studio/qwen-warm"));
    o.cursor = 1;
    match send(&mut o, &rig, ch('x')) {
        OverlayEvent::ClearRole { role, scope } => {
            assert_eq!((role.as_str(), scope), ("small", Scope::Global))
        }
        other => panic!("{other:?}"),
    }
    o.cursor = ROLES.iter().position(|(role, _)| *role == "coach").unwrap();
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Delete)),
        OverlayEvent::None
    ));
    assert!(draw(&mut o, &rig, 120, 30).contains("coach already uses main"));
    o.cursor = 0;
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Delete)),
        OverlayEvent::None
    ));
    assert!(draw(&mut o, &rig, 130, 30).contains("main model cannot be cleared"));
}

#[test]
fn discovery_arriving_later_keeps_search_cursor_and_group() {
    let rig = Rig::new();
    let mut o = ModelOverlay::new(None, roles_with_main(), Scope::Global);
    assert!(draw(&mut o, &rig, 120, 30).contains("Scanning providers and models"));
    let mut early = discovery();
    early.models.truncate(3);
    o.set_discovery(early);
    for e in typed("qwen") {
        send(&mut o, &rig, e);
    }
    send(&mut o, &rig, key(KeyCode::Down));
    assert_eq!(o.highlighted().unwrap().label, "qwen-chat");
    let mut later = discovery();
    later.models.insert(
        0,
        entry(
            "lm-studio",
            "qwen-new",
            "LM Studio · loaded",
            true,
            Some(4096),
        ),
    );
    o.set_discovery(later);
    assert_eq!(o.search, "qwen");
    assert_eq!(
        o.highlighted().unwrap().label,
        "qwen-chat",
        "the same model stays highlighted even though rows were added above it"
    );
    // A group that disappears falls back to All.
    send(&mut o, &rig, key(KeyCode::Esc));
    send(&mut o, &rig, key(KeyCode::Tab));
    for _ in 0..6 {
        send(&mut o, &rig, key(KeyCode::Down));
    }
    assert_eq!(o.group, Group::Provider("openai".into()));
    let mut smaller = discovery();
    smaller.providers.retain(|p| p.id != "openai");
    o.set_discovery(smaller);
    assert_eq!(o.group, Group::All);
}

#[test]
fn kind_legend_omits_whole_items_when_space_is_limited() {
    let o = ModelOverlay::new(Some(Discovery::default()), vec![], Scope::Global);
    let theme = Theme::dark();
    let full = o.kind_line(&theme, 200);
    let full: String = full.spans.iter().map(|s| s.content.as_ref()).collect();
    let unknown = full.find("  ? unknown").unwrap();
    let width = text::width(&full[..unknown]) + 5;
    let narrow = o.kind_line(&theme, width);
    let narrow: String = narrow.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(!narrow.contains('?'), "{narrow}");
    assert!(text::width(&narrow) <= width);
    assert!(full.contains("? unknown"));
}

#[test]
fn empty_states_explain_themselves() {
    let rig = Rig::new();
    let mut o = ModelOverlay::new(Some(Discovery::default()), vec![], Scope::Global);
    let shown = draw(&mut o, &rig, 120, 30);
    assert!(
        shown.contains("No models yet") && shown.contains("set up a provider"),
        "{shown}"
    );
    // With nothing discovered, Enter leads to provider setup instead of doing nothing.
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::Action(a) if a == "open:providers"
    ));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Down)),
        OverlayEvent::None
    ));
    let mut o = overlay();
    send(&mut o, &rig, key(KeyCode::Right));
    send(&mut o, &rig, key(KeyCode::Right));
    send(&mut o, &rig, key(KeyCode::Tab));
    for _ in 0..5 {
        send(&mut o, &rig, key(KeyCode::Down));
    }
    send(&mut o, &rig, key(KeyCode::Tab));
    assert_eq!(o.group, Group::Provider("ollama".into()));
    assert_eq!(o.kind, KindFilter::Embedding);
    assert!(names(&o).is_empty());
    let shown = draw(&mut o, &rig, 120, 30);
    assert!(
        shown.contains("No embedding models here") && shown.contains("Get models"),
        "{shown}"
    );
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    send(&mut o, &rig, key(KeyCode::Right));
    assert_eq!(o.kind, KindFilter::Other);
    assert!(draw(&mut o, &rig, 120, 30).contains("No models in this view"));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
}

#[test]
fn remapped_keys_drive_the_selector_and_the_footer() {
    let mut rig = Rig::new();
    rig.keys.rebind(Action::SelectDown, &["Ctrl+J"]).unwrap();
    rig.keys.rebind(Action::SelectConfirm, &["Ctrl+O"]).unwrap();
    let mut o = overlay();
    send(&mut o, &rig, key(KeyCode::Down));
    assert_eq!(o.cursor, 0, "old chord is unbound");
    send(&mut o, &rig, ctrl('j'));
    assert_eq!(o.cursor, 1);
    assert!(matches!(
        send(&mut o, &rig, ctrl('o')),
        OverlayEvent::AssignModel { .. }
    ));
    assert!(draw(&mut o, &rig, 160, 30).contains("Ctrl+O set main"));
}

#[test]
fn refresh_keys_ask_for_a_rescan() {
    let rig = Rig::new();
    let mut o = overlay();
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::F(5))),
        OverlayEvent::Refresh
    ));
}

#[test]
fn tiny_terminals_never_panic() {
    std::thread::scope(|scope| {
        for w in 0..=30u16 {
            scope.spawn(move || {
                let rig = Rig::new();
                for h in 0..=30u16 {
                    let mut o = overlay();
                    draw(&mut o, &rig, w, h);
                    send(&mut o, &rig, key(KeyCode::Down));
                    send(&mut o, &rig, ctrl('a'));
                    draw(&mut o, &rig, w, h);
                    send(&mut o, &rig, key(KeyCode::Esc));
                    o.show_roles();
                    draw(&mut o, &rig, w, h);
                    for e in typed("qwen") {
                        send(&mut o, &rig, e);
                    }
                    draw(&mut o, &rig, w, h);
                    let mut none = ModelOverlay::new(None, vec![], Scope::Project);
                    draw(&mut none, &rig, w, h);
                }
            });
        }
    });
}

#[test]
fn long_and_hostile_model_names_stay_inside_their_columns() {
    let rig = Rig::new();
    let mut d = discovery();
    d.models.push(entry(
        "lm-studio",
        "日本語のとても長いモデル名\u{1b}[31mred",
        "LM Studio",
        true,
        Some(8192),
    ));
    d.models.push(entry(
        "lm-studio",
        &"x".repeat(300),
        "LM Studio",
        true,
        None,
    ));
    let mut o = ModelOverlay::new(Some(d), roles_with_main(), Scope::Global);
    for w in [40u16, 70, 120] {
        let shown = draw(&mut o, &rig, w, 24);
        assert!(!shown.contains('\u{1b}'));
        for line in shown.lines() {
            assert!(text::width(line) <= usize::from(w) * 2, "{line}");
        }
    }
}

#[test]
fn get_models_is_reachable_from_the_groups_column_by_keyboard() {
    let rig = Rig::new();
    let mut o = overlay();
    assert_eq!(o.group, Group::All);
    // Left moves focus to the groups column; Down walks All -> Favorites -> Memory -> Get models.
    send(&mut o, &rig, key(KeyCode::Left));
    assert_eq!(o.focus, Focus::Groups);
    for _ in 0..3 {
        send(&mut o, &rig, key(KeyCode::Down));
    }
    assert_eq!(o.group, Group::Download);
    let shown = draw(&mut o, &rig, 120, 30);
    assert!(shown.contains("Get models"), "{shown}");
    assert!(shown.contains("Embedding"), "{shown}");
}

#[test]
fn get_models_short_panel_shows_recommendations_before_any_input() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_downloads();
    let repo = crate::llm::download::recommended(crate::llm::download::default_family())[0]
        .repo
        .clone();
    let shown = draw(&mut o, &rig, 120, 10);
    assert!(shown.contains(&repo), "{shown}");
    assert!(o.downloads.idle_now());
    assert!(o.downloads.query_text().is_empty());
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(
        o.downloads.idle_now(),
        "idle fixture must not search on Enter"
    );
    assert!(draw(&mut o, &rig, 120, 10).contains("Downloads are unavailable"));
}

#[test]
fn get_models_selection_style_survives_column_composition() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_downloads();
    let recommendations = crate::llm::download::recommended(crate::llm::download::default_family());
    for selected in 0..2 {
        let buffer = render(160, 30, |frame| {
            o.draw(
                frame,
                &DrawCtx {
                    theme: &rig.theme,
                    keys: &rig.keys,
                    status: &rig.status,
                    preview_status: &status_preview,
                },
            );
        });
        let first = style_of(&buffer, &recommendations[selected].repo).unwrap();
        let other = style_of(&buffer, &recommendations[1 - selected].repo).unwrap();
        assert_eq!(first.bg, chrome::selected_style(&rig.theme).bg);
        assert_ne!(
            first.bg, other.bg,
            "the selected model must be visibly distinct"
        );
        send(&mut o, &rig, key(KeyCode::Down));
    }
}

#[test]
fn typing_in_get_models_edits_its_query_and_never_starts_a_download() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_downloads();
    for c in "qwen".chars() {
        assert!(matches!(send(&mut o, &rig, ch(c)), OverlayEvent::None));
    }
    assert_eq!(o.downloads.query_text(), "qwen");
    assert!(o.downloads.idle_now());
    assert!(o.search.is_empty(), "the models search is not touched");
    assert!(!o.take_refresh());
}

#[test]
fn enter_on_an_embedding_catalog_row_asks_for_its_setup() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_downloads();
    o.downloads.show_embedding_for_test();
    // Discovered embedders lead; the fixture's only one is not served, so Enter stays here.
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    send(&mut o, &rig, key(KeyCode::Down));
    match send(&mut o, &rig, key(KeyCode::Enter)) {
        OverlayEvent::SetupDep { id } => {
            assert_eq!(
                id,
                format!("embeddings:{}", crate::setup::embeddings::CATALOG[0].name)
            );
        }
        other => panic!("expected the embeddings setup, got {other:?}"),
    }
}

#[test]
fn switching_groups_keeps_a_running_download() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_downloads();
    let (cancel, done) = o.downloads.fake_download("org/model");
    // Left reaches the groups column without cancelling the download.
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Left)),
        OverlayEvent::None
    ));
    assert_eq!(o.focus, Focus::Groups);
    send(&mut o, &rig, key(KeyCode::Up));
    assert_eq!(o.group, Group::Memory);
    send(&mut o, &rig, key(KeyCode::Up));
    assert_eq!(o.group, Group::Favorites);
    send(&mut o, &rig, key(KeyCode::Up));
    assert_eq!(o.group, Group::All);
    assert!(!cancel.load(std::sync::atomic::Ordering::SeqCst));
    assert!(o.downloads.is_running());
    done.send(Ok("/no/such/path".into())).unwrap();
}

#[test]
fn model_tabs_use_right_and_tab_while_left_returns_to_groups() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_downloads();
    for code in [KeyCode::Right, KeyCode::Right, KeyCode::Tab] {
        send(&mut o, &rig, key(code));
        assert_eq!(o.focus, Focus::List);
        assert_eq!(o.group, Group::Download);
    }
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Left)),
        OverlayEvent::None
    ));
    assert_eq!(o.focus, Focus::Groups);
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Up)),
        OverlayEvent::None
    ));
    assert_eq!(o.group, Group::Memory);
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Down)),
        OverlayEvent::None
    ));
    assert_eq!(o.group, Group::Download);
    send(&mut o, &rig, key(KeyCode::Enter));
    assert_eq!(o.focus, Focus::List);
    // Keep the previous Ctrl+Left shortcut as an alias.
    send(
        &mut o,
        &rig,
        Event::Key(KeyEvent::new(
            KeyCode::Left,
            crossterm::event::KeyModifiers::CONTROL,
        )),
    );
    assert_eq!(o.focus, Focus::Groups);
}

#[test]
fn get_models_escape_returns_to_groups_then_closes() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_downloads();
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert_eq!(o.focus, Focus::Groups);
    assert_eq!(o.group, Group::Download);
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Up)),
        OverlayEvent::None
    ));
    assert_eq!(o.group, Group::Memory);
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Down)),
        OverlayEvent::None
    ));
    assert_eq!(o.group, Group::Download);
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::Close
    ));
}

#[test]
fn get_models_escape_clears_query_before_returning_to_groups() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_downloads();
    send(&mut o, &rig, ch('q'));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert!(o.downloads.query_text().is_empty());
    assert_eq!(o.focus, Focus::List);
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert_eq!(o.focus, Focus::Groups);
}

#[test]
fn get_models_escape_closes_when_groups_are_hidden() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_downloads();
    draw(&mut o, &rig, 40, 20);
    assert!(o.narrow);
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::Close
    ));
    assert_eq!(o.focus, Focus::List);
}

#[test]
fn show_downloads_opens_the_get_models_group_for_local_models_command() {
    let mut o = overlay();
    o.show_downloads();
    assert_eq!(o.group, Group::Download);
    assert_eq!(o.focus, Focus::List);
    assert!(o.search.is_empty());
}

fn priced(mut entry: ModelEntry, prompt: f64, completion: f64) -> ModelEntry {
    entry.metadata.pricing = Some(crate::llm::catalog::TokenPricing {
        prompt,
        completion,
        cache_read: None,
        cache_write: None,
    });
    entry
}

fn usage(requests: u64, median: Option<f64>, cost_usd: f64) -> ModelUsage {
    let mut usage = ModelUsage::default();
    usage.totals.requests = requests;
    usage.totals.priced_requests = if cost_usd > 0.0 { requests } else { 0 };
    usage.totals.cost_usd = cost_usd;
    usage.totals.avg_latency_ms = Some(1500.0);
    usage.speed.samples = Some(requests);
    usage.speed.median_tok_per_s = median;
    usage
}

fn ctrl(c: char) -> Event {
    Event::Key(KeyEvent::new(
        KeyCode::Char(c),
        crossterm::event::KeyModifiers::CONTROL,
    ))
}

#[test]
fn f2_cycles_the_show_filter_and_escape_resets_it_before_closing() {
    let rig = Rig::new();
    let mut o = overlay();
    send(&mut o, &rig, key(KeyCode::F(2)));
    assert_eq!(o.show, ShowFilter::Available);
    let shown = names(&o);
    assert!(shown.contains(&"llama3".to_owned()), "{shown:?}");
    assert!(!shown.contains(&"gpt-cached".to_owned()), "{shown:?}");
    assert!(!shown.contains(&"nomic-embed".to_owned()), "{shown:?}");
    send(&mut o, &rig, key(KeyCode::F(2)));
    assert_eq!(o.show, ShowFilter::Loaded);
    assert_eq!(names(&o), vec!["qwen-warm".to_owned()]);
    assert!(draw(&mut o, &rig, 120, 24).contains("show  loaded"));

    // A filter matching nothing says which filter hides the models.
    o.show = ShowFilter::Priced;
    assert!(names(&o).is_empty());
    assert!(draw(&mut o, &rig, 120, 24).contains("No models match the \"priced\" filter"));

    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert_eq!(o.show, ShowFilter::Any);
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::Close
    ));
}

#[test]
fn f3_sorts_by_price_speed_and_use_with_unknown_values_last() {
    let rig = Rig::new();
    let mut d = discovery();
    d.models[1] = priced(d.models[1].clone(), 3e-6, 15e-6);
    d.models[2] = priced(d.models[2].clone(), 0.15e-6, 0.6e-6);
    let mut o = ModelOverlay::new(Some(d), roles_with_main(), Scope::Global);
    o.usage.insert("llama3".into(), usage(3, Some(20.0), 0.0));
    o.usage
        .insert("qwen-chat".into(), usage(9, Some(80.0), 0.09));
    o.kind = KindFilter::Chat;

    send(&mut o, &rig, key(KeyCode::F(3)));
    assert_eq!(o.sort, SortOrder::Name);
    // Names sort as shown: `provider/model`.
    assert_eq!(
        names(&o),
        vec!["qwen-chat", "qwen-warm", "llama3", "gpt-cached"]
    );
    send(&mut o, &rig, key(KeyCode::F(3)));
    assert_eq!(o.sort, SortOrder::Price);
    assert_eq!(names(&o)[..2], ["qwen-warm", "qwen-chat"]);
    send(&mut o, &rig, key(KeyCode::F(3)));
    assert_eq!(o.sort, SortOrder::Speed);
    assert_eq!(names(&o)[..2], ["qwen-chat", "llama3"]);
    o.sort = SortOrder::Usage;
    assert_eq!(names(&o)[..2], ["qwen-chat", "llama3"]);
    o.sort = SortOrder::Context;
    assert_eq!(names(&o)[0], "qwen-warm");

    o.show = ShowFilter::Used;
    o.sort = SortOrder::Default;
    assert_eq!(names(&o), vec!["llama3", "qwen-chat"]);
}

#[test]
fn wide_rows_show_price_per_million_and_speed_and_the_numbers_line_shows_use() {
    let rig = Rig::new();
    let mut d = discovery();
    d.models[1] = priced(d.models[1].clone(), 3e-6, 15e-6);
    let mut o = ModelOverlay::new(Some(d), roles_with_main(), Scope::Global);
    o.usage
        .insert("qwen-chat".into(), usage(4, Some(42.0), 0.04));
    let at = names(&o).iter().position(|n| n == "qwen-chat").unwrap();
    o.cursor = at;
    let screen = draw(&mut o, &rig, 140, 24);
    assert!(screen.contains("$3/$15"), "{screen}");
    assert!(screen.contains("42 t/s"), "{screen}");
    assert!(
        screen.contains("price per 1M tokens: in $3 out $15"),
        "{screen}"
    );
    assert!(screen.contains("speed 42 t/s median of 4"), "{screen}");
    assert!(screen.contains("avg $0.010/request"), "{screen}");
    assert!(screen.contains("4 requests"), "{screen}");

    // A model without price or recorded use shows dashes, never an estimate.
    o.cursor = names(&o).iter().position(|n| n == "llama3").unwrap();
    let screen = draw(&mut o, &rig, 140, 24);
    assert!(screen.contains("price -"), "{screen}");
    assert!(screen.contains("no recorded requests"), "{screen}");

    // Narrow panes drop the new columns rather than squeezing the names.
    let narrow = draw(&mut o, &rig, 70, 24);
    assert!(!narrow.contains("$3/$15"), "{narrow}");
}

/// The fixture plus models that run on this machine: an Ollama server on loopback and a local MLX
/// checkpoint (no URL).
fn discovery_with_local_models() -> Discovery {
    let mut d = discovery();
    let mut loopback = entry("ollama", "gemma-local", "Ollama", true, None);
    loopback.selection.base_url = Some("http://127.0.0.1:11434/v1".into());
    let mut mlx = entry("mlx", "qwen-mlx", "Downloaded", true, None);
    mlx.selection.base_url = None;
    d.models.extend([loopback, mlx]);
    d
}

#[test]
fn memory_group_lists_local_chat_models_and_enter_assigns_the_memory_role() {
    let rig = Rig::new();
    let mut o = ModelOverlay::new(
        Some(discovery_with_local_models()),
        roles_with_main(),
        Scope::Global,
    );
    o.set_memory_backend("local");
    o.show_memory();
    assert_eq!(o.group, Group::Memory);
    let mut shown = names(&o);
    shown.sort();
    assert_eq!(
        shown,
        ["gemma-local", "qwen-mlx"],
        "cloud and embedding models are left out"
    );
    assert_eq!(o.count_in(&Group::Memory), 2);
    let screen = draw(&mut o, &rig, 140, 24);
    assert!(screen.contains("MEMORY MODELS"), "{screen}");
    assert!(screen.contains("backend local"), "{screen}");
    assert!(screen.contains("memory role: unassigned"), "{screen}");
    assert!(screen.contains("set memory"), "{screen}");
    assert!(screen.contains("Ctrl+G get memory models"), "{screen}");

    o.cursor = names(&o).iter().position(|n| n == "qwen-mlx").unwrap();
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::AssignModel { role, selection, .. }
            if role == "memory" && selection.model == "qwen-mlx"
    ));
}

#[test]
fn memory_group_keeps_an_assigned_cloud_memory_model_visible() {
    let d = discovery_with_local_models();
    let cloud = d.models[0].selection.clone();
    let mut roles = roles_with_main();
    let memory = roles.iter_mut().find(|r| r.role == "memory").unwrap();
    assign(memory, cloud);
    let mut o = ModelOverlay::new(Some(d), roles, Scope::Global);
    o.show_memory();
    let shown = names(&o);
    assert_eq!(shown[0], "llama3", "the assigned model leads: {shown:?}");
    assert!(!shown.contains(&"qwen-chat".to_owned()), "{shown:?}");
    assert_eq!(shown.len(), 3, "{shown:?}");
}

#[test]
fn memory_group_without_local_models_points_to_get_models() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_memory();
    assert!(names(&o).is_empty());
    let screen = draw(&mut o, &rig, 160, 24);
    assert!(screen.contains("No local chat models"), "{screen}");
}

#[test]
fn ctrl_g_in_the_memory_group_opens_the_memory_shelf_of_get_models() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_memory();
    send(&mut o, &rig, ctrl('g'));
    assert_eq!(o.group, Group::Download);
    let screen = draw(&mut o, &rig, 140, 30);
    assert!(
        screen.contains("Small models for memory"),
        "the Memory shelf is shown: {screen}"
    );
}

#[test]
fn picking_the_memory_role_from_roles_lands_in_the_memory_group() {
    let rig = Rig::new();
    let mut o = overlay();
    o.show_roles();
    o.cursor = ROLES
        .iter()
        .position(|(role, _)| *role == "memory")
        .unwrap();
    send(&mut o, &rig, key(KeyCode::Enter));
    assert_eq!(o.target_role.as_deref(), Some("memory"));
    assert_eq!(o.group, Group::Memory);
}
