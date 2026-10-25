use super::*;
use serde_json::json;

const KEY: [u8; KEY_BYTES] = [7; KEY_BYTES];

fn masker(values: &[&str], env: &[&str]) -> Masker {
    Masker::new(
        &KEY,
        &values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>(),
        env.iter().map(|v| (*v).to_owned()).collect(),
    )
}

fn chat(messages: Vec<Value>) -> Chat {
    Chat {
        messages,
        tools: vec![json!({"type":"function","function":{"name":"bash"}})],
        temperature: 0.0,
        max_tokens: 16,
        seed: None,
    }
}

fn masked_text(masker: &Masker, text: &str) -> (String, Restorer) {
    let (masked, restorer) = masker.mask_chat(&chat(vec![json!({"role":"user","content":text})]));
    (
        masked.messages[0]["content"].as_str().unwrap().to_owned(),
        restorer,
    )
}

#[test]
fn placeholders_are_stable_keyed_and_shaped() {
    let a = masker(&[], &[]);
    let secret = "correct-horse-battery";
    let placeholder = a.placeholder(secret);
    assert_eq!(placeholder.len(), PLACEHOLDER_LEN);
    assert!(placeholder.starts_with("$$SECRET_") && placeholder.ends_with("$$"));
    assert_eq!(placeholder, masker(&[], &[]).placeholder(secret));
    assert_ne!(placeholder, a.placeholder("correct-horse-batterz"));
    let other_key = Masker::new(&[9; KEY_BYTES], &[], Vec::new());
    assert_ne!(placeholder, other_key.placeholder(secret));
}

#[test]
fn each_secret_source_is_masked_and_restored() {
    let github = format!("ghp_{}", "a1B2".repeat(9));
    let m = masker(&["explicit-secret-value"], &["env-secret-value-1"]);
    let text = format!("gh {github} env env-secret-value-1 list explicit-secret-value ok");
    let (masked, restorer) = masked_text(&m, &text);
    for secret in [
        github.as_str(),
        "env-secret-value-1",
        "explicit-secret-value",
    ] {
        assert!(!masked.contains(secret), "{masked}");
        assert!(masked.contains(&m.placeholder(secret)), "{masked}");
    }
    assert!(masked.starts_with("gh $$SECRET_") && masked.ends_with(" ok"));
    assert_eq!(restorer.restore_str(&masked).unwrap(), text);
}

#[test]
fn bearer_introducer_stays_and_identifiers_survive() {
    let m = masker(&[], &[]);
    let token = "abcdefghijklmnopqrstuvwxyz012345";
    let (masked, _) = masked_text(
        &m,
        &format!("Authorization: Bearer {token} token_expiry_seconds=3600"),
    );
    assert_eq!(
        masked,
        format!(
            "Authorization: Bearer {} token_expiry_seconds=3600",
            m.placeholder(token)
        )
    );
}

#[test]
fn short_values_are_ignored() {
    let m = masker(&["short", "1234567"], &["tiny"]);
    let (masked, restorer) = masked_text(&m, "short 1234567 tiny");
    assert_eq!(masked, "short 1234567 tiny");
    assert!(restorer.is_empty());
}

#[test]
fn duplicate_and_overlapping_secrets_leave_no_fragment() {
    let longer = "token-abcdefgh-extra";
    let m = masker(&["token-abcdefgh", longer, longer], &["token-abcdefgh"]);
    let (masked, restorer) = masked_text(&m, &format!("{longer} and token-abcdefgh and {longer}"));
    assert!(!masked.contains("token-"), "{masked}");
    assert_eq!(masked.matches(&m.placeholder(longer)).count(), 2);
    assert_eq!(masked.matches(&m.placeholder("token-abcdefgh")).count(), 1);
    assert_eq!(
        restorer.restore_str(&masked).unwrap(),
        format!("{longer} and token-abcdefgh and {longer}")
    );
    // Two literals that overlap partially merge into one span.
    let m = masker(&["abcdefghij", "ghijklmnop"], &[]);
    let (masked, _) = masked_text(&m, "abcdefghijklmnop");
    assert!(
        !masked.contains("abc") && !masked.contains("nop"),
        "{masked}"
    );
}

#[test]
fn history_is_masked_everywhere_but_tool_definitions_and_bookkeeping() {
    let m = masker(&["hunter2-hunter2"], &[]);
    let request = chat(vec![
        json!({"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function",
            "function":{"name":"bash","arguments":"{\"cmd\":\"echo hunter2-hunter2\"}"}}]}),
        json!({"role":"tool","tool_call_id":"c1","content":"out hunter2-hunter2"}),
        json!({"role":"assistant","content":"x","dial_provider_content":[
            {"type":"text","text":"say hunter2-hunter2","signature":"hunter2-hunter2"}]}),
    ]);
    let (masked, restorer) = m.mask_chat(&request);
    let wire = serde_json::to_string(&masked.messages).unwrap();
    let placeholder = m.placeholder("hunter2-hunter2");
    assert_eq!(wire.matches("hunter2-hunter2").count(), 1, "{wire}");
    assert_eq!(
        masked.messages[2]["dial_provider_content"][0]["signature"],
        "hunter2-hunter2"
    );
    assert!(
        masked.messages[0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .contains(&placeholder)
    );
    assert_eq!(masked.tools, request.tools);
    assert_eq!(
        request.messages[1]["content"], "out hunter2-hunter2",
        "input is not mutated"
    );
    assert!(!restorer.is_empty());
}

#[test]
fn placeholders_in_tool_arguments_are_restored_including_nested_json() {
    let secret = "pa\"ss\\word-123456";
    let m = masker(&[secret], &[]);
    let (_, restorer) = masked_text(&m, secret);
    let placeholder = m.placeholder(secret);
    let mut arguments = json!({
        "command": format!("curl -H 'x: {placeholder}'"),
        "nested": [{"body": json!({"password": placeholder}).to_string()}],
        "number": 3,
    });
    assert!(restorer.restore_value(&mut arguments));
    assert_eq!(arguments["command"], format!("curl -H 'x: {secret}'"));
    let body: Value =
        serde_json::from_str(arguments["nested"][0]["body"].as_str().unwrap()).unwrap();
    assert_eq!(body["password"], secret, "escaping stays valid JSON");
    assert_eq!(arguments["number"], 3);
}

#[test]
fn unknown_or_malformed_placeholders_are_left_alone() {
    let m = masker(&["real-secret-value"], &[]);
    let (_, restorer) = masked_text(&m, "real-secret-value");
    let invented = m.placeholder("never-sent-secret");
    let text = format!("{invented} $$SECRET_12$$ $$SECRET_");
    assert!(restorer.restore_str(&text).is_none());
}

#[test]
fn replies_are_restored_in_text_and_calls() {
    let m = masker(&["real-secret-value"], &[]);
    let (_, restorer) = masked_text(&m, "real-secret-value");
    let p = m.placeholder("real-secret-value");
    let mut reply = Reply {
        text: format!("use {p}"),
        tool_calls: vec![super::super::ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({"cmd": format!("echo {p}")}),
            malformed_arguments: false,
        }],
        ..Reply::default()
    };
    restorer.restore_reply(&mut reply);
    assert_eq!(reply.text, "use real-secret-value");
    assert_eq!(
        reply.tool_calls[0].arguments["cmd"],
        "echo real-secret-value"
    );
}

#[test]
fn streamed_text_is_restored_across_chunk_boundaries() {
    let m = masker(&["real-secret-value"], &[]);
    let (_, restorer) = masked_text(&m, "real-secret-value");
    let p = m.placeholder("real-secret-value");
    let full = format!("a ${{}} then {p} and $ end");
    let full = full.replace("${}", "$5");
    for split in 1..full.len() {
        if !full.is_char_boundary(split) {
            continue;
        }
        let mut seen = String::new();
        let mut sink = |chunk: &str| {
            seen.push_str(chunk);
            true
        };
        let (a, b) = full.split_at(split);
        let reply = restorer
            .run(Some(&mut sink), |on_text| {
                let on_text = on_text.unwrap();
                on_text(a);
                on_text(b);
                Ok(Reply {
                    text: full.clone(),
                    ..Reply::default()
                })
            })
            .unwrap();
        assert_eq!(
            seen, "a $5 then real-secret-value and $ end",
            "split {split}"
        );
        assert_eq!(reply.text, seen);
    }
}

#[test]
fn disabled_config_builds_no_masker_and_touches_no_key() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::default();
    assert!(!config.enabled);
    assert!(Masker::from_config(&config, dir.path()).unwrap().is_none());
    assert!(!key_path(dir.path()).exists());
}

#[test]
fn install_key_is_created_once_private_and_reused() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        enabled: true,
        env: false,
        values: vec!["explicit-secret-value".into()],
    };
    let first = Masker::from_config(&config, dir.path()).unwrap().unwrap();
    let again = Masker::from_config(&config, dir.path()).unwrap().unwrap();
    assert_eq!(
        first.placeholder("explicit-secret-value"),
        again.placeholder("explicit-secret-value")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(key_path(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
    std::fs::write(key_path(dir.path()), "garbage").unwrap();
    let error = Masker::from_config(&config, dir.path()).err().unwrap();
    assert!(
        error.to_string().contains("secret-masking key"),
        "{error:#}"
    );
}

#[test]
fn environment_secrets_come_from_the_process_only_when_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let off = Config {
        enabled: true,
        env: false,
        values: Vec::new(),
    };
    assert!(
        Masker::from_config(&off, dir.path())
            .unwrap()
            .unwrap()
            .literals
            .is_empty()
    );
}
