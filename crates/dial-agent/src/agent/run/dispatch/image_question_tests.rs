use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;

#[cfg(unix)]
#[test]
fn pre_tool_use_hook_denies_image_questions_like_plain_reads() {
    let overlay = r#"
[[hooks]]
event = "pre_tool_use"
matcher = "read"
command = ["sh", "-c", "cat >/dev/null; printf '%s' '{\"decision\":\"block\",\"reason\":\"read gate\"}'"]
timeout_secs = 5
"#;
    struct Quiet;
    impl Ui for Quiet {}
    with_agent(overlay, |agent| {
        let mut backend = agent.backend.take().unwrap();
        for path in ["img.png", "img.png?q=what+is+this"] {
            let output = agent.dispatch(
                &mut backend,
                &ToolCall {
                    id: path.into(),
                    name: "read".into(),
                    arguments: json!({"path": path}),
                    malformed_arguments: false,
                },
                &mut Quiet,
            );
            assert!(output.is_error, "{path}: {}", output.text);
            assert!(output.text.contains("read gate"), "{path}: {}", output.text);
        }
        agent.backend = Some(backend);
    });
}

#[test]
fn only_image_paths_with_a_question_are_image_questions() {
    let read = |path: &str| ToolCall {
        id: String::new(),
        name: "read".into(),
        arguments: json!({"path": path}),
        malformed_arguments: false,
    };
    assert_eq!(
        image_question(&read("photos/cat.jpg?q=what breed")),
        Some(("photos/cat.jpg", "what breed"))
    );
    assert_eq!(image_question(&read("notes.md?q=x")), None);
    assert_eq!(image_question(&read("photos/cat.jpg")), None);
}
