use super::*;

fn file_item(path: &str, diff: &str) -> HashMap<String, String> {
    HashMap::from([
        ("path".to_string(), path.to_string()),
        ("diff".to_string(), diff.to_string()),
    ])
}

#[test]
fn every_family_renders_with_empty_context() {
    for family in [
        PromptFamily::Analysis,
        PromptFamily::Fast,
        PromptFamily::Map,
        PromptFamily::Reduce,
        PromptFamily::Summary,
        PromptFamily::SummaryRewrite,
    ] {
        let rendered = render_conventional_prompt(family, &PromptContext::default());
        assert!(!rendered.user.is_empty(), "{family:?} user prompt is empty");
        assert!(
            !rendered.user.contains("{{"),
            "{family:?} left an unrendered tag"
        );
    }
}

#[test]
fn map_items_render_without_escaping() {
    let ctx = PromptContext::default().with_list("files", vec![file_item("a<b.rs", "+x && y")]);
    let rendered = render_conventional_prompt(PromptFamily::Map, &ctx);
    assert!(rendered.user.contains(r#"<file path="a<b.rs">"#));
    assert!(rendered.user.contains("+x && y"));
}

#[test]
fn missing_variables_are_empty_and_conditionals_are_falsy() {
    let rendered = render_conventional_prompt(PromptFamily::Summary, &PromptContext::default());
    assert!(rendered.user.contains("scope: (none)"));
    let with_scope = PromptContext::default().with_text("scope", "cli");
    let rendered = render_conventional_prompt(PromptFamily::Summary, &with_scope);
    assert!(rendered.user.contains("scope: cli"));
}

#[test]
fn standalone_block_lines_leave_no_blank_gaps() {
    let ctx = PromptContext::default().with_text("types_description", "feat: new");
    let rendered = render_conventional_prompt(PromptFamily::Analysis, &ctx);
    assert!(
        rendered
            .user
            .contains("<commit_types>\nfeat: new\n</commit_types>")
    );
    assert!(!rendered.user.contains("{{"));
}

#[test]
fn post_render_collapses_blank_runs_outside_code_fences() {
    assert_eq!(format_post_render("a\n\n\nb"), "a\nb");
    assert_eq!(format_post_render("a\n\nb"), "a\n\nb");
    assert_eq!(format_post_render("\n\na  \n"), "a");
    assert_eq!(
        format_post_render("```\nx  \n\n\ny\n```"),
        "```\nx\n\n\ny\n```"
    );
}

#[test]
fn system_text_is_not_rendered() {
    let ctx = PromptContext::default().with_text("diff", "D");
    let rendered = render_conventional_prompt(PromptFamily::Fast, &ctx);
    assert!(!rendered.system.contains("{{"));
}
