use super::*;

#[test]
fn computer_menu_select_forwards_options_to_the_exact_window_without_escalation() {
    let (result, natives) = run_worker_with_natives(
        "const target = await desktop.window('12086');
         await target.menu.select(['File', 'New Window']);
         await target.menu.select(['File', 'New Window'], {takeover: false});
         await target.menu.select(['File', 'New Window'], {takeover: true});",
        brave,
        false,
    );
    assert_eq!(result["ok"], true, "{result}");
    let selected: Vec<_> = natives
        .iter()
        .filter(|m| m["method"] == "menuSelect")
        .collect();
    assert_eq!(selected.len(), 3, "no implicit retries");
    for request in &selected {
        assert_eq!(request["args"]["target"], "12086");
        assert_eq!(request["args"]["path"], json!(["File", "New Window"]));
    }
    assert!(selected[0]["args"].get("options").is_none());
    assert_eq!(selected[1]["args"]["options"], json!({"takeover": false}));
    assert_eq!(selected[2]["args"]["options"], json!({"takeover": true}));
}

#[test]
fn computer_menu_select_read_only_and_invalid_paths_never_dispatch() {
    for (code, read_only) in [
        (
            "const w = await desktop.window('12086'); await w.menu.select(['File', 'New Window'], {takeover:true});",
            true,
        ),
        (
            "const w = await desktop.window('12086'); await w.menu.select([], {takeover:true});",
            false,
        ),
    ] {
        let (result, natives) = run_worker_with_natives(code, brave, read_only);
        assert_eq!(result["ok"], false, "{result}");
        assert!(!natives.iter().any(|m| m["method"] == "menuSelect"));
    }
}
