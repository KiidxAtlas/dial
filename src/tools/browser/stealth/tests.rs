use super::*;

#[test]
fn index_json_matches_script_order() {
    let index: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../runtimes/browser/stealth/index.json"
    ))
    .unwrap();
    let listed: Vec<&str> = index["scripts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let loaded: Vec<&str> = STEALTH_SCRIPTS.iter().map(|(name, _)| *name).collect();
    assert_eq!(listed, loaded);
}

#[test]
fn env_overrides_setting_only_for_zero_and_one() {
    assert!(resolve_enabled(false, Some("1")));
    assert!(!resolve_enabled(true, Some("0")));
    assert!(resolve_enabled(true, Some("maybe")));
    assert!(!resolve_enabled(false, None));
    assert!(resolve_enabled(true, None));
}

#[test]
fn edge_keeps_enable_automation() {
    let chrome = ignored_default_args(Some(
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ));
    assert!(chrome.contains(&ENABLE_AUTOMATION_FLAG));
    let edge = ignored_default_args(Some(
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ));
    assert!(!edge.contains(&ENABLE_AUTOMATION_FLAG));
    assert_eq!(edge.len(), chrome.len() - 1);
}

#[test]
fn injection_script_splices_every_patch() {
    let script = injection_script();
    assert!(!script.contains(BOOTSTRAP_SCRIPTS_MARKER));
    assert_eq!(
        script.matches("} catch (e) {}").count(),
        STEALTH_SCRIPTS.len()
    );
}
