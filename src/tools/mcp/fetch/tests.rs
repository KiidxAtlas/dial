use super::*;

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn the_server_command_is_picked_only_when_unambiguous() {
    assert_eq!(pick_command(&names(&["serve"]), &["pkg"]), Ok("serve"));
    assert_eq!(
        pick_command(&names(&["cli", "context_mode"]), &["context-mode"]),
        Ok("context_mode")
    );
    assert_eq!(
        pick_command(&names(&["tool", "tool-mcp"]), &["x"]),
        Ok("tool-mcp")
    );
    assert_eq!(
        pick_command(
            &names(&["headroom", "headroom-cache-ttl"]),
            &["headroom-ai"]
        ),
        Err("headroom, headroom-cache-ttl".to_owned())
    );
}

#[test]
fn names_and_versions_reject_shell_and_option_shapes() {
    assert!(plain_name("mcp-server_git.v2"));
    assert!(!plain_name("--from"));
    assert!(!plain_name("a b"));
    assert!(!plain_name("x;rm"));
    assert!(plain_version("1.0.0-beta.1+b7"));
    assert!(!plain_version("1.0 || 2"));
}
