use super::*;

fn config(chain: &[&str], jina: bool) -> WebConfig {
    WebConfig {
        reader_chain: chain.iter().map(|name| (*name).to_owned()).collect(),
        jina_reader: jina,
        ..WebConfig::default()
    }
}

#[test]
fn default_chain_and_jina_gating() {
    assert_eq!(
        chain(&config(&[], false)).unwrap(),
        vec!["native", "trafilatura", "lynx"]
    );
    assert_eq!(
        chain(&config(&["jina", "native"], false)).unwrap(),
        vec!["native"]
    );
    assert_eq!(
        chain(&config(&["Jina", "native"], true)).unwrap(),
        vec!["jina", "native"]
    );
}

#[test]
fn unknown_reader_is_an_error() {
    let error = chain(&config(&["magic"], false)).unwrap_err();
    assert!(error.contains("magic"), "{error}");
}

#[test]
fn gate_rejects_short_and_js_gated_output() {
    let prose = "This sentence has plenty of ordinary words to read. ".repeat(4);
    assert!(passes_gate(&prose));
    assert!(!passes_gate("too short"));
    let gated = format!("Please enable JavaScript to continue. {}", "x ".repeat(80));
    assert!(!passes_gate(&gated));
}

#[test]
fn gate_rejects_navigation_like_line_walls() {
    let nav = (0..20)
        .map(|index| format!("Link {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(is_low_quality(&nav));
    let prose =
        "A paragraph line that is long enough to count as ordinary prose text here.\n".repeat(12);
    assert!(!is_low_quality(&prose));
}

#[test]
fn missing_external_reader_fails_so_the_chain_falls_through() {
    let workspace = Path::new(".");
    assert!(
        run_external(
            &Client::new(),
            workspace,
            "dial-no-such-reader-xyz",
            &[],
            "<p>x</p>"
        )
        .is_err()
    );
}
