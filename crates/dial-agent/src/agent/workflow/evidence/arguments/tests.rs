use super::*;

fn history() -> Vec<Value> {
    vec![
        json!({"role":"assistant","tool_calls":[{"id":"write-one","function":{"name":"write","arguments":json!({"path":"output.c","content":"source body\n".repeat(4000)}).to_string()}}]}),
        json!({"role":"tool","tool_call_id":"write-one","content":"actual write receipt","is_error":false,"dial_execution_status":"ok"}),
        json!({"role":"user","content":"newer context ".repeat(5000)}),
    ]
}

#[test]
fn settled_argument_projection_is_artifact_backed_stable_and_does_not_mutate_history() {
    let dir = tempfile::tempdir().unwrap();
    let artifacts = ArtifactStore::new(dir.path()).unwrap();
    let original = history();
    let mut first = original.clone();
    let mut memo = Supersession::default();
    project_arguments(&mut first, &mut memo, &artifacts, 8192, 40000);
    assert!(
        first[0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .contains("Historical argument body elided")
    );
    assert!(
        original[0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .contains("source body")
    );
    assert_eq!(first[1], original[1], "mutation receipt must remain exact");
    let mut second = original.clone();
    project_arguments(&mut second, &mut memo, &artifacts, 8192, 41000);
    assert_eq!(
        first, second,
        "cached placeholders must stay byte-stable between boundaries"
    );
}

#[test]
fn failed_pending_and_native_replay_arguments_remain_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let artifacts = ArtifactStore::new(dir.path()).unwrap();
    for mode in ["failed", "pending", "native", "off"] {
        let mut messages = history();
        match mode {
            "failed" => messages[1]["is_error"] = json!(true),
            "pending" => {
                messages.remove(1);
            }
            "native" => {
                messages[0]["dial_provider_content"] =
                    json!([{"type":"thinking","signature":"protected"}])
            }
            _ => {}
        }
        let original = messages.clone();
        project_arguments(
            &mut messages,
            &mut Supersession::default(),
            &artifacts,
            if mode == "off" { 0 } else { 8192 },
            40000,
        );
        assert_eq!(messages, original, "{mode}");
    }
}
