use super::*;

#[test]
fn listed_window_screenshot_targets_the_selected_id() {
    let (result, natives) = run_worker_with_natives(
        "const [w] = await desktop.windows({ title: 'clickup' });
         try { await w.screenshot(); } catch (e) { return [w.width, e.message]; }",
        brave,
        true,
    );
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(
        result["payload"]["returnValue"],
        json!([10, "capture refused"])
    );
    let capture = natives.iter().find(|n| n["method"] == "capture").unwrap();
    assert_eq!(capture["args"]["target"], "12086");
}

#[test]
fn capture_alias_uses_only_the_requested_screenshot_target() {
    let (result, natives) = run_worker_with_natives(
        "const errors = [];
         for (const target of [desktop, await desktop.display('active'), await desktop.window('12086')]) {
           try { await target.capture({silent: true}); } catch (e) { errors.push(e.message); }
         }
         return errors;",
        brave,
        true,
    );
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(
        result["payload"]["returnValue"],
        json!(vec!["capture refused"; 3])
    );
    let targets: Vec<_> = natives
        .iter()
        .filter(|n| n["method"] == "capture")
        .map(|n| n["args"]["target"].as_str().unwrap())
        .collect();
    assert_eq!(targets, ["desktop", "display:active", "12086"]);
}

#[test]
fn computer_explicit_id_resolves_beyond_a_truncated_listing() {
    let result = run_worker(
        "const listed = await desktop.windows();
         const text = await desktop.window('60');
         const num = await desktop.window(60);
         let diagnostic;
         try { await desktop.window({ title: 'missing' }); } catch (e) { diagnostic = e.message; }
         return { count: listed.length, isArray: Array.isArray(listed), ids: [text.id, num.id], diagnostic };",
        |listing| {
            let windows: Vec<Value> = (1..=60).map(|id| window(&id.to_string(), "fixture")).collect();
            if listing == 0 {
                json!(windows)
            } else {
                json!({"windows": &windows[..48], "truncated": true, "limit": 48})
            }
        },
    );
    assert_eq!(result["ok"], true, "{result}");
    let value = &result["payload"]["returnValue"];
    assert_eq!(value["count"], 48);
    assert_eq!(value["isArray"], true);
    assert_eq!(value["ids"], json!(["60", "60"]));
    let diagnostic = value["diagnostic"].as_str().unwrap();
    assert!(
        diagnostic.contains("truncated at 48 windows"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("lookup bypasses this listing cap"),
        "{diagnostic}"
    );
}

#[test]
fn computer_explicit_id_preserves_permission_failure_without_listing_fallback() {
    let result = run_worker(
        "try { await desktop.window('60'); } catch (e) { return e.message; }",
        |listing| {
            assert_eq!(listing, 0, "explicit lookup must not enumerate windows");
            json!({"name": "Error", "message": "macOS Screen Recording permission is not granted for this process"})
        },
    );
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(
        result["payload"]["returnValue"],
        "macOS Screen Recording permission is not granted for this process"
    );
}
