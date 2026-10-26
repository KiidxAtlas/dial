use super::*;
use crate::llm::{catalog, local_server};
use serde_json::json;

/// Tests bind port 0, so a later test can get the port an earlier one used. The process-wide
/// server caches must not answer for the new server, or the probe its scripts expect never comes
/// and every later script is served to the wrong request.
#[test]
fn a_server_on_a_reused_port_is_probed_afresh() {
    let llama = json!({"total_slots":4,"default_generation_settings":{"n_ctx":4096}});
    let first = serve(vec![super::json(200, &llama)]);
    let base = format!("{}/v1", first.url);
    let facts = local_server::facts("llama-cpp", "m", Some(&base), None, &[]);
    assert_eq!(facts.slots, Some(4));
    assert!(catalog::is_llama_server(&base, None, &[]), "cached");
    assert_eq!(first.requests().len(), 1);

    let address = first.url.trim_start_matches("http://");
    let listener = TcpListener::bind(address).expect("rebind the released port");
    let not_found = || super::json(404, &json!({}));
    let second = serve_on(listener, vec![not_found(), not_found()]);
    assert_eq!(second.url, first.url);
    assert_eq!(
        local_server::facts("llama-cpp", "m", Some(&base), None, &[]).slots,
        None
    );
    assert!(!catalog::is_llama_server(&base, None, &[]));
    assert_eq!(
        second.requests().len(),
        2,
        "both lookups asked the new server"
    );
}

#[test]
fn origins_match_on_a_port_boundary_only() {
    let origin = "http://127.0.0.1:5000";
    assert!(crate::llm::same_origin("http://127.0.0.1:5000", origin));
    assert!(crate::llm::same_origin("http://127.0.0.1:5000/v1", origin));
    assert!(!crate::llm::same_origin(
        "http://127.0.0.1:50001/v1",
        origin
    ));
    assert!(!crate::llm::same_origin("http://127.0.0.2:5000/v1", origin));
}

/// A client that stops before using every script must fail the test that waits on the server,
/// not block it forever in `accept`.
#[test]
fn requests_give_up_when_scripted_connections_stop_arriving() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let fake = serve_with_idle(
        listener,
        vec![super::json(200, &json!({})), super::json(200, &json!({}))],
        Duration::from_millis(200),
    );
    let response = ureq::get(&format!("{}/one", fake.url)).call().unwrap();
    assert_eq!(response.status(), 200);
    let started = Instant::now();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fake.requests()))
        .expect_err("one of two scripts was never requested");
    assert!(started.elapsed() < Duration::from_secs(10));
    let message = panic.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(
        message.contains("1 of 2 scripted connections arrived"),
        "{message}"
    );
}
