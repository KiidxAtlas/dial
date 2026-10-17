use super::*;
#[cfg(unix)]
#[test]
fn stdio_plugin_handshake_and_event_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("plugin.sh");
    std::fs::write(
        &program,
        r#"#!/bin/sh
read request
case "$request" in
  *initialize*) printf '{"jsonrpc":"2.0","id":1,"result":{"events":["pre_tool_use"]}}\n' ;;
  *) printf '{"jsonrpc":"2.0","id":1,"result":{"rewrite":{"input":{"command":"safe"} }}}\n' ;;
esac
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let plugin = ProcessPlugin::start("fixture", &program, &[], &Default::default(), 2).unwrap();
    assert_eq!(plugin.events(), ["pre_tool_use"]);
    let rewrite = plugin
        .on_event("pre_tool_use", &json!({}), &AtomicBool::new(false))
        .unwrap();
    assert_eq!(rewrite, Some(json!({"input":{"command":"safe"}})));
}
