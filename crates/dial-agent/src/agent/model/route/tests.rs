use super::*;

fn on_fallback_until(until: Instant) -> ActiveRoute {
    ActiveRoute {
        tried: 1,
        fallback: Some(Fallback {
            until,
            primary_cap: Some(8192),
            served: false,
        }),
        promoted: None,
    }
}

#[test]
fn fallback_is_retained_while_cooling_and_restored_once_elapsed() {
    let now = Instant::now();
    let route = on_fallback_until(now + Duration::from_secs(300));
    assert!(route.on_fallback());
    assert!(route.cooling_at(now));
    assert!(!route.restore_due_at(now));
    // The cooldown boundary itself already allows the primary back.
    let route = on_fallback_until(now);
    assert!(!route.cooling_at(now));
    assert!(route.restore_due_at(now));
}

#[test]
fn primary_route_never_restores_and_reset_forgets_the_episode() {
    let now = Instant::now();
    let mut route = ActiveRoute::default();
    assert!(!route.on_fallback());
    assert!(!route.restore_due_at(now));
    route = on_fallback_until(now);
    assert!(route.restore_due_at(now));
    route.reset();
    assert!(!route.on_fallback());
    assert_eq!(route.tried, 0);
}

// ---- context promotion ----

use crate::llm::fake::{self, Script};

const PROMOTING: &str =
    "[compaction.promotion]\nenabled=true\n[compaction.promotion.targets]\ndefault=['@task']\n";

fn big_role(url: &str, window: u32) -> String {
    format!(
        "[model_roles.task]\nprovider='endpoint'\nmodel='big'\nbase_url='{url}/v1'\ncontext_window={window}\n"
    )
}

/// An interactive agent whose live backend talks to `url`, over default configuration plus `overlay`.
fn with_server_agent<T>(url: &str, overlay: &str, f: impl FnOnce(&mut Agent<'_>) -> T) -> T {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let layout =
        crate::config::paths::Layout::with_user_dir(&project, temp.path().join("home")).unwrap();
    let mut overlay: toml::Table =
        toml::from_str(&format!("{overlay}\n[improve]\nenabled=false")).unwrap();
    overlay.entry("model").or_insert_with(|| "test".into());
    let config = Config::load(&layout, Some(overlay)).unwrap();
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{url}/v1"),
            model: "test".into(),
            api_key: None,
        },
        std::path::PathBuf::new(),
        5,
    );
    let session = crate::storage::session::Session::create(
        &layout,
        crate::storage::session::SessionMeta {
            model: config.model.clone(),
        },
    )
    .unwrap();
    let mut agent = Agent::interactive(
        &config,
        backend,
        layout,
        session,
        None,
        None,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    f(&mut agent)
}

fn with_promotion_agent<T>(window: u32, extra: &str, f: impl FnOnce(&mut Agent<'_>) -> T) -> T {
    let url = "http://127.0.0.1:9";
    with_server_agent(
        url,
        &format!("{PROMOTING}{extra}\n{}", big_role(url, window)),
        f,
    )
}

#[derive(Default)]
struct Events(Vec<String>);
impl Ui for Events {
    fn event(&mut self, event: &crate::agent::ui::ui_event::UiEvent) {
        self.0.push(event.text.clone());
    }
}

fn promote(agent: &mut Agent<'_>, request_tokens: u64, overflow: bool) -> (bool, Events) {
    let mut backend = agent.backend.take().unwrap();
    let mut ui = Events::default();
    let promoted = agent.try_promote_context(&mut backend, request_tokens, 1024, overflow, &mut ui);
    agent.backend = Some(backend);
    (promoted, ui)
}

#[test]
fn promotion_installs_the_larger_model_and_says_so() {
    with_promotion_agent(200_000, "", |agent| {
        let before = agent.context_window();
        assert!(before < 200_000);
        let (promoted, ui) = promote(agent, 40_000, false);
        assert!(promoted);
        assert!(agent.route.promoted.is_some());
        assert_eq!(agent.context_window(), 200_000);
        assert_eq!(agent.auth_config.model, "big");
        assert!(!agent.route.on_fallback());
        assert!(
            ui.0.iter()
                .any(|text| text.contains("instead of compacting"))
        );
        // Nothing larger remains, so a second attempt leaves the route alone.
        assert!(!promote(agent, 40_000, false).0);
        // A user model switch forgets the promotion.
        agent.route.reset();
        assert!(!agent.route.promoted.is_some());
    });
}

#[test]
fn promotion_is_refused_when_disabled_hermetic_or_without_a_larger_fitting_target() {
    // Disabled (the default), even with targets configured.
    let url = "http://127.0.0.1:9";
    let disabled = format!(
        "[compaction.promotion.targets]\ndefault=['@task']\n{}",
        big_role(url, 200_000)
    );
    with_server_agent(url, &disabled, |agent| {
        assert!(!promote(agent, 40_000, false).0);
        assert!(!agent.route.promoted.is_some());
    });
    // Hermetic sessions never switch models.
    with_promotion_agent(200_000, "", |agent| {
        agent.hermetic = true;
        assert!(!promote(agent, 40_000, false).0);
        assert!(!agent.route.promoted.is_some());
    });
    // The target window is not larger (smaller, then exactly equal).
    with_promotion_agent(4_096, "", |agent| {
        assert!(!promote(agent, 1_000, true).0);
        assert!(!agent.route.promoted.is_some());
    });
    let live = with_promotion_agent(4_096, "", |agent| agent.context_window());
    with_promotion_agent(live, "", |agent| {
        assert!(!promote(agent, 1_000, true).0);
        assert!(!agent.route.promoted.is_some());
    });
    // The request would not fit the larger target once its output reserve is counted.
    with_promotion_agent(200_000, "", |agent| {
        let before = agent.context_window();
        assert!(!promote(agent, 199_500, true).0);
        assert!(!agent.route.promoted.is_some());
        assert_eq!(agent.context_window(), before);
        assert_eq!(agent.auth_config.model, "test");
    });
    // The only target is the live model itself.
    with_server_agent(
        url,
        "[compaction.promotion]\nenabled=true\n[compaction.promotion.targets]\ndefault=['test']\n",
        |agent| assert!(!promote(agent, 1_000, true).0),
    );
}

#[test]
fn promotion_clears_a_fallback_and_fallbacks_return_to_the_promoted_model() {
    with_promotion_agent(
        200_000,
        "[model_roles.coach]\nprovider='endpoint'\nmodel='fb'\nbase_url='http://127.0.0.1:9/v1'\n[retry.fallback_chains]\ndefault=['@coach']\n",
        |agent| {
            let mut backend = agent.backend.take().unwrap();
            let mut ui = Events::default();
            let error = anyhow::anyhow!("rate limited");
            assert!(agent.fail_over(&mut backend, &error, &mut ui));
            assert!(agent.route.on_fallback());
            assert_eq!(agent.auth_config.model, "fb");
            // Promotion replaces the fallback and carries no cooldown back to `test`.
            assert!(agent.try_promote_context(&mut backend, 40_000, 1024, false, &mut ui));
            assert!(!agent.route.on_fallback());
            assert_eq!(agent.auth_config.model, "big");
            assert!(
                !agent
                    .route
                    .restore_due_at(Instant::now() + Duration::from_secs(86_400))
            );
            // A later fallback fails over from, and returns to, the promoted model.
            assert!(agent.fail_over(&mut backend, &error, &mut ui));
            assert_eq!(agent.auth_config.model, "fb");
            agent.route.fallback.as_mut().unwrap().until = Instant::now();
            assert!(agent.restore_primary_if_due(&mut backend, &mut ui));
            assert_eq!(agent.auth_config.model, "big");
            assert!(agent.route.promoted.is_some());
            agent.backend = Some(backend);
        },
    );
}

#[test]
fn a_primary_that_cannot_be_rebuilt_keeps_the_fallback_instead_of_failing_the_turn() {
    with_server_agent(
        "http://127.0.0.1:9",
        "[model_roles.coach]\nprovider='endpoint'\nmodel='fb'\nbase_url='http://127.0.0.1:9/v1'\n[retry.fallback_chains]\ndefault=['@coach']\n",
        |agent| {
            let mut backend = agent.backend.take().unwrap();
            let mut ui = Events::default();
            assert!(agent.fail_over(&mut backend, &anyhow::anyhow!("rate limited"), &mut ui));
            // The primary's configuration no longer builds a backend (no model selected).
            let mut broken = agent.effective_config().clone();
            broken.model = String::new();
            agent.route.promoted = Some(Promoted { config: broken });
            agent.route.fallback.as_mut().unwrap().until = Instant::now();
            assert!(!agent.restore_primary_if_due(&mut backend, &mut ui));
            assert!(agent.route.on_fallback());
            assert_eq!(agent.auth_config.model, "fb");
            // The restore is retried after a fresh cooldown, not at every request.
            assert!(agent.route.cooling_at(Instant::now()));
            agent.backend = Some(backend);
        },
    );
}

#[test]
fn never_revert_keeps_the_fallback_after_its_cooldown_until_a_model_switch() {
    with_server_agent(
        "http://127.0.0.1:9",
        "[retry]\nfallback_revert='never'\n[model_roles.coach]\nprovider='endpoint'\nmodel='fb'\nbase_url='http://127.0.0.1:9/v1'\n[retry.fallback_chains]\ndefault=['@coach']\n",
        |agent| {
            let mut backend = agent.backend.take().unwrap();
            let mut ui = Events::default();
            assert!(agent.fail_over(&mut backend, &anyhow::anyhow!("rate limited"), &mut ui));
            agent.route.fallback.as_mut().unwrap().until = Instant::now();
            assert!(!agent.restore_primary_if_due(&mut backend, &mut ui));
            assert!(agent.route.on_fallback());
            assert_eq!(agent.auth_config.model, "fb");
            agent.backend = Some(backend);
        },
    );
}

#[test]
fn cooldown_expiry_restores_the_primary_and_announces_it_once() {
    with_server_agent(
        "http://127.0.0.1:9",
        "[endpoint]\nbase_url='http://127.0.0.1:9/v1'\n[model_roles.coach]\nprovider='endpoint'\nmodel='fb'\nbase_url='http://127.0.0.1:9/v1'\n[retry.fallback_chains]\ndefault=['@coach']\n",
        |agent| {
            let mut backend = agent.backend.take().unwrap();
            let mut ui = Events::default();
            assert!(agent.fail_over(&mut backend, &anyhow::anyhow!("rate limited"), &mut ui));
            // Failing over alone never returns to the primary within the same failure.
            assert_eq!(agent.auth_config.model, "fb");
            let cooldown_end = agent.route.fallback.as_ref().unwrap().until;
            assert!(
                agent
                    .route
                    .cooling_at(cooldown_end - Duration::from_millis(1))
            );
            agent.route.fallback.as_mut().unwrap().until = Instant::now();
            assert!(agent.restore_primary_if_due(&mut backend, &mut ui));
            assert_eq!(agent.auth_config.model, "test");
            assert!(
                ui.0.iter()
                    .any(|text| text.contains("returning to the primary"))
            );
            agent.backend = Some(backend);
        },
    );
}

#[test]
fn the_first_fallback_reply_is_announced_once() {
    with_server_agent(
        "http://127.0.0.1:9",
        "[model_roles.coach]\nprovider='endpoint'\nmodel='fb'\nbase_url='http://127.0.0.1:9/v1'\n[retry.fallback_chains]\ndefault=['@coach']\n",
        |agent| {
            let mut backend = agent.backend.take().unwrap();
            let mut ui = Events::default();
            agent.note_fallback_served(&mut ui);
            assert!(
                ui.0.is_empty(),
                "no fallback installed, nothing to announce"
            );
            assert!(agent.fail_over(&mut backend, &anyhow::anyhow!("rate limited"), &mut ui));
            agent.note_fallback_served(&mut ui);
            agent.note_fallback_served(&mut ui);
            let served = ui.0.iter().filter(|text| text.contains("answered")).count();
            assert_eq!(served, 1);
            assert!(agent.route.fallback.as_ref().unwrap().served);
            agent.backend = Some(backend);
        },
    );
}

fn sse(text: &str) -> Script {
    let event = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
    Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

#[test]
fn threshold_promotion_replaces_compaction_for_a_request_over_the_trigger() {
    let server = fake::serve(vec![sse("done on the big model")]);
    let overlay = format!("{PROMOTING}{}", big_role(&server.url, 200_000));
    with_server_agent(&server.url, &overlay, |agent| {
        // About 30k estimated tokens: over the small window's trigger, far under the big one's.
        let report = agent
            .turn(&"lorem ipsum ".repeat(10_000), &mut NullUi)
            .unwrap();
        assert_eq!(report.text, "done on the big model");
        assert!(agent.route.promoted.is_some());
        assert_eq!(agent.context_window(), 200_000);
    });
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        1,
        "no compaction request precedes the answer"
    );
    assert_eq!(fake::body(&requests[0])["model"], "big");
}

#[test]
fn overflow_promotes_and_retries_on_the_larger_model_before_compacting() {
    let server = fake::serve(vec![
        fake::json(
            400,
            &json!({"error":{"code":"context_length_exceeded","message":"This model's maximum context length is exceeded"}}),
        ),
        sse("answered after promotion"),
    ]);
    let overlay = format!("{PROMOTING}{}", big_role(&server.url, 200_000));
    with_server_agent(&server.url, &overlay, |agent| {
        let report = agent.turn("hello", &mut NullUi).unwrap();
        assert_eq!(report.text, "answered after promotion");
        assert!(agent.route.promoted.is_some());
    });
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(fake::body(&requests[0])["model"], "test");
    assert_eq!(fake::body(&requests[1])["model"], "big");
}
