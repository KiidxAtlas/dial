use super::*;

fn start(o: &mut SettingsOverlay, rig: &Rig) {
    assert!(matches!(
        send(
            o,
            &rig.keys,
            Event::Key(KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL))
        ),
        OverlayEvent::None
    ));
}

#[test]
fn bulk_cancel_confirm_and_failure_wait_for_acknowledgement() {
    let rig = Rig::new();
    let saved = table("harness.max_steps = 12\nui.show_tools = true");
    let mut o = SettingsOverlay::new(
        saved.clone(),
        saved.clone(),
        Table::new(),
        Scope::Project,
        vec![],
    )
    .with_project_name("sample");
    o.changed_only = true;
    o.search = "harness.max_steps".into();
    start(&mut o, &rig);
    assert_eq!(
        o.batch_expected()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["harness.max_steps"]
    );
    assert!(draw(&mut o, &rig, 100, 30).contains("harness.max_steps ->"));
    assert!(draw(&mut o, &rig, 100, 30).contains("project (sample)"));
    send(&mut o, &rig.keys, key(KeyCode::Esc));
    assert!(o.batch_expected().is_none());
    assert_eq!(o.project, saved);
    start(&mut o, &rig);
    draw(&mut o, &rig, 100, 30);
    // First Enter opens a separate confirmation, it never emits a mutation.
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(draw(&mut o, &rig, 100, 30).contains("Enter confirms removal"));
    let OverlayEvent::UnsetMany { paths, scope } = send(&mut o, &rig.keys, key(KeyCode::Enter))
    else {
        panic!("expected reviewed batch");
    };
    assert_eq!(paths, ["harness.max_steps"]);
    assert_eq!(scope, Scope::Project);
    assert_eq!(o.project, saved);
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    o.batch_settled(Err("disk full".to_owned()));
    assert_eq!(o.project, saved);
    assert!(o.batch_expected().is_some());
    assert!(draw(&mut o, &rig, 100, 30).contains("Failed: disk full"));
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    draw(&mut o, &rig, 100, 30);
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::UnsetMany { .. }
    ));
    let remaining = table("ui.show_tools = true");
    o.batch_settled(Ok((remaining.clone(), remaining.clone(), Table::new())));
    assert_eq!(o.project, remaining);
    assert!(o.batch_expected().is_none());
}

#[test]
fn tiny_review_cannot_authorize_hidden_removal_and_scope_stays_frozen() {
    let rig = Rig::new();
    for (w, h) in [(30, 30), (100, 10)] {
        let mut o = overlay();
        o.global = table("harness.max_steps = 12");
        o.changed_only = true;
        start(&mut o, &rig);
        draw(&mut o, &rig, w, h);
        for _ in 0..3 {
            assert!(matches!(
                send(&mut o, &rig.keys, key(KeyCode::Enter)),
                OverlayEvent::None
            ));
        }
        send(&mut o, &rig.keys, ctrl('s'));
        assert_eq!(o.scope, Scope::Global);
        draw(&mut o, &rig, 100, 30);
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        draw(&mut o, &rig, 100, 30);
        send(&mut o, &rig.keys, Event::Resize(w, h));
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        draw(&mut o, &rig, w, h);
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        send(&mut o, &rig.keys, key(KeyCode::Esc));
        assert!(o.batch_expected().is_none());
    }
}

#[test]
fn filtered_embedding_pair_never_adds_hidden_saved_paths() {
    let rig = Rig::new();
    let mut o = overlay();
    o.global = table(
        "memory.semantic.embedding_model = 'test'\nmemory.semantic.embedding_base_url = 'http://localhost:8000'",
    );
    o.changed_only = true;
    o.search = EMBEDDING_URL_PATH.into();
    start(&mut o, &rig);
    assert!(o.batch_expected().is_none());
    assert!(o.feedback.as_deref().unwrap().contains("show both"));
}
