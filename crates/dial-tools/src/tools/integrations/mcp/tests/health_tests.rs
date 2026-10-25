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
