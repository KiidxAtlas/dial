//! End-to-end check of the Python plugin SDK (`sdk/python`) against Dial's own configuration
//! loader, configured-tool dispatch and hook runner. Needs `python3` 3.9+; skipped (with a
//! message) when it is not installed.

use super::*;
use crate::agent::safety::hooks;
use std::process::Command;

fn generated_config() -> Option<Config> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let example = root.join("sdk/python/examples/word_tools.py");
    let output = match Command::new("python3").arg(&example).arg("config").output() {
        Ok(output) => output,
        Err(error) => {
            eprintln!("skipping: python3 is unavailable ({error})");
            return None;
        }
    };
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let config: Config = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    config.validate().unwrap();
    Some(config)
}

#[test]
fn sdk_tools_run_through_configured_tool_dispatch() {
    let Some(config) = generated_config() else {
        return;
    };
    let cancel = AtomicBool::new(false);
    let count = &config.tools["words_count"];
    assert!(count.description.starts_with("Count the words"));
    let out = custom_tool_output(count, &json!({"text":"a b a","unique":true}), &cancel);
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        serde_json::from_str::<Value>(&out.text).unwrap(),
        json!({"unit":"words","count":2})
    );
    // Dial checks required arguments before starting the program.
    let missing = custom_tool_output(count, &json!({}), &cancel);
    assert!(missing.is_error);
    assert!(missing.text.contains("missing required argument `text`"));
    // A `ToolError` reaches the model through the failure message's stderr tail.
    let failed = custom_tool_output(&config.tools["words_join"], &json!({"parts":[]}), &cancel);
    assert!(failed.is_error);
    assert!(
        failed.text.contains("(exit 1): parts is empty"),
        "{}",
        failed.text
    );
}

#[test]
fn sdk_hooks_block_and_pass_through_dials_hook_runner() {
    let Some(config) = generated_config() else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let interrupt = AtomicBool::new(false);
    let mut notices = Vec::new();
    let (verdict, _) = hooks::pre_tool_noted(
        &config.hooks,
        root.path(),
        "write",
        &json!({"path":"app/.env","content":"x"}),
        &interrupt,
        &mut |notice| notices.push(notice.to_owned()),
    );
    let reason = verdict.unwrap_err();
    assert!(reason.contains(".env files is not allowed"), "{reason}");
    let (verdict, _) = hooks::pre_tool_noted(
        &config.hooks,
        root.path(),
        "write",
        &json!({"path":"src/main.rs","content":"x"}),
        &interrupt,
        &mut |notice| notices.push(notice.to_owned()),
    );
    assert_eq!(verdict, Ok(None), "{notices:?}");
    // The matcher keeps the hook away from other tools.
    let (verdict, _) = hooks::pre_tool_noted(
        &config.hooks,
        root.path(),
        "read",
        &json!({"path":".env"}),
        &interrupt,
        &mut |notice| notices.push(notice.to_owned()),
    );
    assert_eq!(verdict, Ok(None), "{notices:?}");
}
