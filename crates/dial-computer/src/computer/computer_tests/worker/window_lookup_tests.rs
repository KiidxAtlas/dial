use super::*;

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
