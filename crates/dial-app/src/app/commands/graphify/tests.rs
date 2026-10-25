use super::*;

#[test]
fn views_parse_with_tree_as_default_and_reject_unknown_words() {
    assert_eq!(View::parse(""), Some(View::Tree));
    assert_eq!(View::parse("tree"), Some(View::Tree));
    assert_eq!(View::parse("html"), Some(View::Html));
    assert_eq!(View::parse("callflow"), Some(View::Callflow));
    assert_eq!(View::parse("svg"), None);
    assert_eq!(View::parse("tree extra"), None);
}

#[test]
fn every_subcommand_in_the_spec_is_a_view() {
    for sub in COMMANDS[0].subcommands {
        assert!(View::parse(sub).is_some(), "{sub}");
    }
}

#[test]
fn output_dir_follows_graphify_out_like_graphify_does() {
    let root = Path::new("/work/project");
    let out = |value: Option<&str>| out_dir(root, value.map(OsStr::new));
    assert_eq!(out(None), root.join("graphify-out"));
    // Blank, the project itself and its parents fall back to the default.
    for ignored in ["", "  ", ".", "./", "..", "../..", "a/../..", "a/.."] {
        assert_eq!(out(Some(ignored)), root.join("graphify-out"), "{ignored:?}");
    }
    assert_eq!(out(Some(" shared ")), root.join("shared"));
    assert_eq!(out(Some("../sibling")), root.join("../sibling"));
    #[cfg(unix)]
    assert_eq!(out(Some("/tmp/graphs")), PathBuf::from("/tmp/graphs"));
}

#[test]
fn exporter_arguments_name_the_graph_and_the_page_they_write() {
    let graph = Path::new("/p/graphify-out/graph.json");
    let page = |view: View| Path::new("/p/graphify-out").join(view.file());
    let args = |view: View| -> Vec<String> {
        view.args(graph, &page(view))
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(
        args(View::Tree),
        [
            "tree",
            "--graph",
            "/p/graphify-out/graph.json",
            "--output",
            "/p/graphify-out/GRAPH_TREE.html"
        ]
    );
    assert_eq!(
        args(View::Callflow),
        [
            "export",
            "callflow-html",
            "--graph",
            "/p/graphify-out/graph.json",
            "--output",
            "/p/graphify-out/callflow.html"
        ]
    );
    // `export html` has no output flag; it writes graph.html beside the graph.
    assert_eq!(
        args(View::Html),
        ["export", "html", "--graph", "/p/graphify-out/graph.json"]
    );
    assert_eq!(graph.with_file_name(View::Html.file()), page(View::Html));
}
