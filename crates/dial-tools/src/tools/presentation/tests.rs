use super::*;
#[test]
fn coherent_runtime_batched_reads_and_patch_targets_use_the_tool_contract() {
    assert_eq!(
        render_call(
            "read",
            &serde_json::json!({"paths":["src/a.rs:1-80","src/b.rs"]})
        )
        .target,
        "src/a.rs:1-80, src/b.rs"
    );
    let input = "*** Begin Patch\n*** Add File: src/new.rs\n+fn main() {}\n*** Delete File: src/old.rs\n*** End Patch";
    assert_eq!(
        render_call("edit", &serde_json::json!({"input":input})).target,
        "src/new.rs, src/old.rs"
    );
    assert_eq!(
        render_call(
            "edit",
            &serde_json::json!({"path":"src/lib.rs","old_string":"old","new_string":"new"})
        )
        .target,
        "src/lib.rs"
    );
    assert_eq!(
        render_call("edit", &serde_json::json!({"input":"malformed"})).target,
        "patch"
    );
}
#[test]
fn coherent_runtime_check_presentation_does_not_guess_from_command_text() {
    let ordinary = render_call("bash", &serde_json::json!({"command":"Verify parser"}));
    assert_eq!(ordinary.label, "Run");
    assert_eq!(ordinary.body, BodyStyle::Text);
    let check = render_call(
        "bash",
        &serde_json::json!({"command":"cargo test","_dial_check":"parser"}),
    );
    assert_eq!(
        (check.label.as_str(), check.target.as_str()),
        ("Check", "parser")
    );
    assert_eq!(check.body, BodyStyle::Quiet);
}
