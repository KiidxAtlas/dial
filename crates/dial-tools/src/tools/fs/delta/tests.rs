use crate::tools::EditMode;
use crate::tools::edit::edit_source::revision_hex;
use crate::tools::testkit::*;
use serde_json::json;
use std::fs;

fn numbered(lines: usize) -> String {
    (1..=lines).map(|n| format!("line {n}\n")).collect()
}

#[test]
fn a_changed_file_sends_only_its_changed_lines() {
    let (dir, ws) = workspace();
    let ws = ws.with_edit_mode(EditMode::Hashline);
    let before = numbered(10);
    fs::write(dir.path().join("f.txt"), &before).unwrap();
    let first = run(&ws, "read", json!({"path": "f.txt:1-10"}));
    assert!(!first.is_error, "{}", first.text);
    // Insert one line after line 3 and change line 8.
    let after = before
        .replace("line 3\n", "line 3\ninserted\n")
        .replace("line 8\n", "line eight\n");
    fs::write(dir.path().join("f.txt"), &after).unwrap();
    let known = [((1, 10), "call_a".to_owned())];
    let args = json!({"path": "f.txt:1-11"});
    let plan = ws
        .read_delta_plan(&args, &revision_hex(&before), &known)
        .expect("a mappable change of a visible read is planned");
    assert_eq!(plan.revision, revision_hex(&after));
    assert_eq!(plan.missing, vec![(4, 4), (9, 9)]);
    let current: Vec<_> = plan
        .covered
        .iter()
        .map(|r| (r.current, r.earlier))
        .collect();
    assert_eq!(
        current,
        vec![((1, 3), (1, 3)), ((5, 8), (4, 7)), ((10, 11), (9, 10))]
    );
    let note = plan.note(&revision_hex(&before));
    assert!(note.contains("5-8 = earlier 4-7 (call call_a)"), "{note}");
    assert!(note.contains(&format!("[f.txt#{}]", plan.tag)), "{note}");
    // Unchanged covered lines are now edit witnesses of the current version.
    let edit = run(
        &ws,
        "edit",
        json!({"input": format!("[f.txt#{}]\nPUT 6.=6:\n+line six\n", plan.tag)}),
    );
    assert!(!edit.is_error, "{}", edit.text);
    assert!(
        fs::read_to_string(dir.path().join("f.txt"))
            .unwrap()
            .contains("line six\n")
    );
}

#[test]
fn no_plan_without_a_change_coverage_or_retained_text() {
    let (dir, ws) = workspace();
    let ws = ws.with_edit_mode(EditMode::Hashline);
    let before = numbered(6);
    fs::write(dir.path().join("f.txt"), &before).unwrap();
    let known = [((1, 6), "call_a".to_owned())];
    // The earlier version was never read, so its text is not retained.
    let args = json!({"path": "f.txt:1-6"});
    fs::write(dir.path().join("f.txt"), before.replace("line 2", "two")).unwrap();
    assert!(
        ws.read_delta_plan(&args, &revision_hex(&before), &known)
            .is_none()
    );
    // Same revision: an ordinary pointer, not a delta.
    let now = fs::read_to_string(dir.path().join("f.txt")).unwrap();
    assert!(!run(&ws, "read", args.clone()).is_error);
    assert!(
        ws.read_delta_plan(&args, &revision_hex(&now), &known)
            .is_none()
    );
    // Nothing visible covers the requested rows.
    fs::write(dir.path().join("f.txt"), now.replace("line 5", "five")).unwrap();
    assert!(
        ws.read_delta_plan(&args, &revision_hex(&now), &[((100, 120), "x".to_owned())])
            .is_none()
    );
    // `fresh: true` always reads every row.
    let fresh = json!({"path": "f.txt:1-6", "fresh": true});
    assert!(
        ws.read_delta_plan(&fresh, &revision_hex(&now), &known)
            .is_none()
    );
    assert!(
        ws.read_delta_plan(&args, &revision_hex(&now), &known)
            .is_some()
    );
}
