use super::*;

fn node(id: &str, parent: Option<&str>, kind: TreeNodeKind, current: bool) -> TreeNode {
    TreeNode {
        id: id.into(),
        parent: parent.map(Into::into),
        depth: usize::from(parent.is_some()),
        kind,
        text: format!("text of {id}"),
        label: (id == "b").then(|| "checkpoint".into()),
        ts: "2026-11-25T08:00:00Z".into(),
        on_active_branch: current,
        current,
        children: 0,
    }
}

#[test]
fn tree_rows_keep_order_links_and_where_the_conversation_stands() {
    let rows = nodes(&[
        node("a", None, TreeNodeKind::User, false),
        node("b", Some("a"), TreeNodeKind::Assistant, true),
        node("c", Some("a"), TreeNodeKind::Summary, false),
    ]);
    let rows = rows.as_array().unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["a", "b", "c"]);
    assert!(rows[0]["parent"].is_null());
    assert_eq!(rows[1]["parent"], "a");
    assert_eq!(rows[1]["depth"], 1);
    assert_eq!(rows[0]["kind"], "user");
    assert_eq!(rows[2]["kind"], "summary");
    assert_eq!(rows[1]["label"], "checkpoint");
    assert!(rows[0]["label"].is_null());
    assert_eq!(rows[1]["current"], true);
    assert_eq!(rows[2]["on_active_branch"], false);
}
