use super::*;

#[test]
fn discovery_prompt_names_the_request() {
    let text = render_discovery("ts errors");
    assert!(text.contains("**ts errors**"));
    assert!(!text.contains("{{request}}"));
}

#[test]
fn assignment_fills_every_placeholder() {
    let assignment = Assignment {
        index: 2,
        write_scope: vec!["a.ts".into()],
        peers: vec![],
        diagnostics: "- [error] a.ts:1 — tsc: boom".into(),
    };
    let text = render_assignment(&assignment, &["tsc --noEmit".into()]);
    assert!(text.contains("Repair worker 2."));
    assert!(text.contains("tsc --noEmit"));
    assert!(!text.contains("{{"));
}
