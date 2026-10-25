use super::*;
use crate::storage::session::tree::{MAX_LABEL_CHARS, TreeNode};

fn ids(tree: &[TreeNode]) -> Vec<&str> {
    tree.iter().map(|n| n.id.as_str()).collect()
}

#[test]
fn linear_conversation_lists_user_messages_without_depth() {
    let mut s = Session::ephemeral();
    let u1 = s.append(user("one")).unwrap();
    s.append(assistant("a1")).unwrap();
    let u2 = s.append(user("two")).unwrap();
    let tree = s.tree();
    assert_eq!(ids(&tree), [u1.as_str(), u2.as_str()]);
    assert!(tree.iter().all(|n| n.depth == 0 && n.on_active_branch));
    assert!(tree[1].current && !tree[0].current);
}

#[test]
fn labels_latest_wins_empty_clears_and_never_reach_the_model() {
    let mut s = Session::ephemeral();
    s.append(user("one")).unwrap();
    let a = s.append(assistant("answer")).unwrap();
    assert_eq!(s.label_leaf("  good   point ").unwrap(), a);
    assert_eq!(s.label_of(&a), Some("good point"));
    s.label_leaf("better").unwrap();
    assert_eq!(s.label_of(&a), Some("better"));
    let before = s.entries.len();
    s.label_leaf("better").unwrap();
    assert_eq!(
        s.entries.len(),
        before,
        "an unchanged label appends nothing"
    );
    assert_eq!(s.messages().len(), 2);
    let tree = s.tree();
    assert!(
        tree.iter()
            .any(|n| n.id == a && n.label.as_deref() == Some("better"))
    );
    s.label_leaf("").unwrap();
    assert_eq!(s.label_of(&a), None);
    assert!(s.label_leaf(&"x".repeat(MAX_LABEL_CHARS + 1)).is_err());
    assert!(Session::ephemeral().label_leaf("x").is_err());
}

#[test]
fn navigating_to_a_user_message_rewinds_before_it_and_keeps_the_old_branch() {
    let mut s = Session::ephemeral();
    let u1 = s.append(user("one")).unwrap();
    s.append(assistant("a1")).unwrap();
    let u2 = s.append(user("two")).unwrap();
    let a2 = s.append(assistant("a2")).unwrap();
    let nav = s.navigate_to(&u2).unwrap();
    assert!(nav.moved);
    assert_eq!(nav.prefill.as_deref(), Some("two"));
    assert_eq!(contents(&s.messages()), ["one", "a1"]);
    // Continue on a new branch; the abandoned one stays in the tree.
    let u3 = s.append(user("three")).unwrap();
    let tree = s.tree();
    assert_eq!(ids(&tree), [u1.as_str(), u2.as_str(), u3.as_str()]);
    let by = |id: &str| tree.iter().find(|n| n.id == id).unwrap();
    assert_eq!(by(&u1).children, 2);
    assert_eq!((by(&u2).depth, by(&u3).depth), (1, 1));
    assert!(!by(&u2).on_active_branch && by(&u3).on_active_branch);
    // Going back to the abandoned branch's answer restores its messages.
    let nav = s.navigate_to(&a2).unwrap();
    assert!(nav.moved && nav.prefill.is_none());
    assert_eq!(contents(&s.messages()), ["one", "a1", "two", "a2"]);
    assert_active_reads(&s);
}

#[test]
fn navigating_to_where_you_already_are_appends_nothing() {
    let mut s = Session::ephemeral();
    s.append(user("one")).unwrap();
    let a1 = s.append(assistant("a1")).unwrap();
    let before = s.entries.len();
    let nav = s.navigate_to(&a1).unwrap();
    assert!(!nav.moved);
    assert_eq!(s.entries.len(), before);
}

#[test]
fn navigation_refuses_unknown_internal_and_mid_tool_entries() {
    let mut s = Session::ephemeral();
    s.append(user("one")).unwrap();
    let call = s
        .append(json!({"role":"assistant","content":"","tool_calls":[{"id":"c","type":"function","function":{"name":"x","arguments":"{}"}}]}))
        .unwrap();
    s.append(json!({"role":"tool","tool_call_id":"c","content":"r"}))
        .unwrap();
    let state = s.append(json!({"role":"dial_state","state":{}})).unwrap();
    assert!(s.navigate_to("nope").is_err());
    assert!(s.navigate_to(&call).is_err());
    assert!(s.navigate_to(&state).is_err());
}

#[test]
fn persisted_navigation_and_labels_survive_reopen() {
    let e = env();
    let mut s = new_session(&e);
    let u1 = s.append(user("one")).unwrap();
    s.append(assistant("a1")).unwrap();
    let u2 = s.append(user("two")).unwrap();
    s.label_leaf("fork here").unwrap();
    s.navigate_to(&u2).unwrap();
    s.append(user("alt")).unwrap();
    let path = s.path().unwrap().to_owned();
    drop(s);
    let s = Session::open(&path).unwrap();
    assert_eq!(contents(&s.messages()), ["one", "a1", "alt"]);
    let tree = s.tree();
    assert_eq!(tree.len(), 3);
    assert_eq!(tree.iter().find(|n| n.id == u1).unwrap().children, 2);
    assert_eq!(s.label_of(&u2), Some("fork here"));
    assert!(tree.iter().any(|n| n.label.as_deref() == Some("fork here")));
    let mut s = s;
    assert!(s.take_recovery_diagnostics().is_empty());
}

#[test]
fn read_only_snapshots_cannot_move_or_label() {
    let e = env();
    let mut s = new_session(&e);
    let u = s.append(user("one")).unwrap();
    s.append(assistant("a")).unwrap();
    let path = s.path().unwrap().to_owned();
    drop(s);
    let mut snapshot = Session::read(&path).unwrap();
    assert!(snapshot.label_leaf("x").is_err());
    assert!(snapshot.navigate_to(&u).is_err());
}
