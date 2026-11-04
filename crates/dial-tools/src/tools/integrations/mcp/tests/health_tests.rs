use super::*;
use crate::runtime::integration::{
    CatalogFreshness, ConnectionHealth, DispatchState, NotificationHealth, Settlement,
};

#[test]
fn health_channels_and_catalog_are_independent_and_transitions_are_claimed_once() {
    let loaded = load_fake();
    let session = &loaded.sessions[0];
    let initial = session.health();
    assert_eq!(initial.connection, ConnectionHealth::Connected);
    assert_eq!(initial.notifications, NotificationHealth::Healthy);
    assert_eq!(initial.catalog, CatalogFreshness::Fresh);

    session.on_signal(Signal::NotificationDegraded);
    let degraded = session.health();
    assert_eq!(degraded.connection, ConnectionHealth::Connected);
    assert_eq!(degraded.notifications, NotificationHealth::Degraded);
    assert_eq!(degraded.catalog, CatalogFreshness::Unknown);
    session.on_signal(Signal::NotificationDegraded);
    assert_eq!(session.health().transitions, degraded.transitions);

    session.on_signal(Signal::NotificationHealthy);
    assert_eq!(
        session.health().catalog,
        CatalogFreshness::Unknown,
        "channel recovery cannot prove catalog freshness"
    );
    session.on_signal(Signal::ToolsChanged);
    let changed = session.health();
    assert_eq!(changed.connection, ConnectionHealth::Connected);
    assert_eq!(changed.notifications, NotificationHealth::Healthy);
    assert_eq!(changed.catalog, CatalogFreshness::Stale);
    assert_eq!(
        session.observe_health(changed.transitions),
        changed.transitions
    );
    assert_eq!(session.observe_health(changed.transitions), 0);
    assert_eq!(session.observe_health(initial.transitions), 0);
}

#[test]
fn stale_catalog_revokes_read_only_exemption_and_refuses_old_definition() {
    let mut server = fake_config("", 30);
    server.trust_read_only_hint = true;
    let loaded = load(&config_with(vec![("fake", server)]));
    let echo = tool(&loaded, "mcp__fake__echo");
    assert!(echo.read_only_hint());
    assert!(!echo.needs_approval(&json!({})));
    loaded.sessions[0].on_signal(Signal::ToolsChanged);
    *loaded.sessions[0].catalog_retry.lock() = Some(Instant::now() + Duration::from_secs(1));
    assert!(!echo.read_only_hint());
    assert!(echo.needs_approval(&json!({})));
    let result = call(
        &loaded,
        "mcp__fake__echo",
        json!({"text":"must not dispatch"}),
    );
    assert!(result.is_error);
    assert_eq!(
        result.execution.effects,
        crate::runtime::execution::Effects::None
    );
    let facts = result.execution.integration.unwrap();
    assert_eq!(facts.dispatch, DispatchState::NotDispatched);
    assert_eq!(facts.health.catalog, CatalogFreshness::Stale);
}

#[test]
fn unchanged_reconnected_catalog_restores_freshness_with_new_generation() {
    let loaded = load_fake();
    let session = &loaded.sessions[0];
    let generation = session.health().generation;
    session.on_signal(Signal::ToolsChanged);
    let _turn = session.turn.lock();
    session.open_locked(&|| false).unwrap();
    let health = session.health();
    assert!(health.generation > generation);
    assert_eq!(health.catalog, CatalogFreshness::Fresh);
    assert_eq!(health.connection, ConnectionHealth::Connected);
}

#[test]
fn cancelled_listing_does_not_use_cache_or_dispatch() {
    let loaded = load_fake();
    let session = &loaded.sessions[0];
    session.listing.lock().resources = Some(json!({"resources":[]}));
    session.listing.lock().prompts = Some(json!({"prompts":[]}));
    assert!(session.resources_until(&|| true).is_err());
    assert!(session.prompts_until(&|| true).is_err());
}

#[test]
fn inventory_and_tool_outcomes_expose_independent_health_facts() {
    let config = config_with(vec![("fake", fake_config("", 30))]);
    let loaded = load(&config);
    let (_dir, workspace) = testkit::workspace();
    let inventory = loaded.inventory_tool(&config);
    let output = inventory.call(
        &ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        },
        &json!({"action":"list"}),
    );
    let data = output.data.unwrap();
    assert_eq!(data["servers"][0]["health"]["connection"], "connected");
    assert_eq!(data["servers"][0]["health"]["notifications"], "healthy");
    assert_eq!(data["servers"][0]["health"]["catalog"], "fresh");
    let first = call(&loaded, "mcp__fake__echo", json!({"text":"hello"}));
    let facts = first.execution.integration.unwrap();
    assert_eq!(facts.dispatch, DispatchState::Dispatched);
    assert_eq!(facts.settlement, Settlement::Reply);
    assert!(facts.state_changes > 0);
    let second = call(&loaded, "mcp__fake__echo", json!({}));
    assert_eq!(second.execution.integration.unwrap().state_changes, 0);
}

#[test]
fn recovered_notification_channel_revalidates_unknown_catalog_without_reload() {
    let mut server = fake_config("", 30);
    server.trust_read_only_hint = true;
    let loaded = load(&config_with(vec![("fake", server)]));
    let session = &loaded.sessions[0];
    let generation = session.health().generation;
    session.on_signal(Signal::NotificationDegraded);
    session.on_signal(Signal::NotificationHealthy);
    assert_eq!(session.health().catalog, CatalogFreshness::Unknown);
    let output = call(&loaded, "mcp__fake__echo", json!({"text":"after rotation"}));
    assert!(!output.is_error, "{}", output.text);
    assert_eq!(session.health().generation, generation);
    assert_eq!(session.health().catalog, CatalogFreshness::Fresh);
    assert!(tool(&loaded, "mcp__fake__echo").read_only_hint());
}

#[test]
fn stale_catalog_retries_are_cancellable_rate_limited_and_recover_without_reconnect() {
    let loaded = load(&config_with(vec![(
        "fake",
        fake_config("list-fails-once", 30),
    )]));
    let session = &loaded.sessions[0];
    let generation = session.health().generation;
    session.on_signal(Signal::ToolsChanged);
    assert!(session.client_until(&|| true).is_err());
    assert_eq!(session.health().catalog, CatalogFreshness::Stale);
    assert!(
        session.client_until(&|| false).is_err(),
        "second listing fails transiently"
    );
    let limited = call(&loaded, "mcp__fake__plain", json!({}));
    assert!(
        limited.is_error,
        "cooldown must not dispatch a stale definition"
    );
    assert_eq!(
        limited.execution.integration.unwrap().dispatch,
        DispatchState::NotDispatched
    );
    // Move only the retry clock, without waiting on wall time or restarting the transport.
    *session.catalog_retry.lock() = Some(Instant::now());
    let recovered = call(&loaded, "mcp__fake__plain", json!({}));
    assert!(!recovered.is_error, "{}", recovered.text);
    assert_eq!(session.health().catalog, CatalogFreshness::Fresh);
    assert_eq!(session.health().generation, generation);
}

#[test]
fn catalog_refresh_does_not_erase_an_invalidation_during_listing() {
    let loaded = load_fake();
    let session = &loaded.sessions[0];
    let calls = std::cell::Cell::new(0);
    let cancel = || {
        calls.set(calls.get() + 1);
        if calls.get() == 2 {
            session.on_signal(Signal::ToolsChanged);
        }
        false
    };
    session
        .sync_catalog(
            &session.current(),
            Some(Instant::now() + Duration::from_secs(5)),
            &cancel,
        )
        .unwrap();
    assert!(calls.get() >= 2);
    assert_eq!(session.health().catalog, CatalogFreshness::Stale);
}

#[test]
fn catalog_admission_wait_is_cancellable_without_holding_request_locks() {
    let loaded = load_fake();
    let session = &loaded.sessions[0];
    let client = session.current();
    let remote = session.catalog_snapshot().1.pop().unwrap();
    let _catalog = session.catalog.lock();
    let cancelled = std::cell::Cell::new(false);
    let result = session.admit_tool(&client, &remote, &|| cancelled.replace(true));
    assert!(result.unwrap_err().to_string().contains("cancelled"));
}

struct ListingTimeouts(Mutex<Vec<(String, Duration)>>);
impl client::Remote for ListingTimeouts {
    fn post(
        &self,
        message: &Value,
        timeout: Duration,
        operation: &Arc<client::lifecycle::Operation>,
        sink: &mut dyn FnMut(Value),
    ) -> anyhow::Result<()> {
        let method = message["method"].as_str().unwrap();
        self.0.lock().push((method.to_owned(), timeout));
        assert!(operation.dispatch());
        operation.delivered();
        let key = match method {
            "resources/list" => "resources",
            "resources/templates/list" => "resourceTemplates",
            "prompts/list" => "prompts",
            other => panic!("unexpected method {other}"),
        };
        sink(json!({"id":message["id"], "result":{key:[]}}));
        Ok(())
    }
}

#[test]
fn background_surface_refresh_bounds_zero_timeout_without_changing_foreground_policy() {
    let loaded = load(&config_with(vec![("fake", fake_config("", 0))]));
    let session = &loaded.sessions[0];
    let transport = Arc::new(ListingTimeouts(Mutex::default()));
    // Keep the original transport alive: dropping it would legitimately signal Closed to
    // the session supervisor, unrelated to this isolated listing transport fixture.
    let _previous = std::mem::replace(
        &mut *session.client.lock(),
        Arc::new(Client::remote(
            "fake",
            transport.clone(),
            Arc::new(Shared::new(vec![], None)),
        )),
    );
    session.relist(&Signal::ResourcesChanged);
    session.relist(&Signal::PromptsChanged);
    assert!(
        session.notices.lock().is_empty(),
        "{:?}",
        session.notices.lock()
    );
    let background = std::mem::take(&mut *transport.0.lock());
    assert_eq!(background.len(), 3, "{background:?}");
    assert!(
        background
            .iter()
            .all(|(_, timeout)| !timeout.is_zero() && *timeout <= CONNECT_TIMEOUT),
        "{background:?}"
    );
    session.listing.lock().reset();
    session.resources().unwrap();
    session.prompts().unwrap();
    let foreground = transport.0.lock();
    assert_eq!(foreground.len(), 3, "{foreground:?}");
    assert!(
        foreground.iter().all(|(_, timeout)| timeout.is_zero()),
        "{foreground:?}"
    );
}

#[test]
fn bounded_surface_refresh_deadline_also_cancels_connection_lock_wait() {
    let loaded = load(&config_with(vec![("fake", fake_config("", 0))]));
    let session = &loaded.sessions[0];
    for resources in [true, false] {
        thread::scope(|scope| {
            let guard = session.turn.lock();
            let (tx, rx) = std::sync::mpsc::channel();
            scope.spawn(move || {
                let deadline = Some(Instant::now() + Duration::from_millis(100));
                let result = if resources {
                    session.list_resources(deadline, &|| false)
                } else {
                    session.list_prompts(deadline, &|| false)
                };
                tx.send(result.is_err()).unwrap();
            });
            let outcome = rx.recv_timeout(Duration::from_secs(2));
            drop(guard);
            assert!(
                outcome.unwrap(),
                "deadline must stop the refresh before the connection lock becomes available"
            );
        });
    }
}
