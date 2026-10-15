//! Prompt templates for conventional commit generation.
//!
//! Prompt rendering on top of the subset of the
//! Handlebars dialect the templates use: `{{ var }}` (no HTML escaping, missing
//! values render empty), `{{#if var}}…{{else}}…{{/if}}` and
//! `{{#each list}}…{{/each}}` with item fields. Standalone block-tag lines are
//! removed as Handlebars does, and the output then goes through the same
//! whitespace normalization applied after rendering.

use std::collections::HashMap;

const USER_SEPARATOR: &str = "<!-- USER -->";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptFamily {
    Analysis,
    Fast,
    Map,
    Reduce,
    Summary,
    SummaryRewrite,
}

/// System and user halves of one rendered prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderedPrompt {
    pub(crate) system: String,
    pub(crate) user: String,
}

/// Values available to a template: scalar text and lists of item records.
#[derive(Debug, Default, Clone)]
pub(crate) struct PromptContext {
    text: HashMap<String, String>,
    lists: HashMap<String, Vec<HashMap<String, String>>>,
}

impl PromptContext {
    pub(crate) fn with_text(mut self, key: &str, value: impl Into<String>) -> Self {
        self.text.insert(key.to_string(), value.into());
        self
    }

    pub(crate) fn with_list(mut self, key: &str, items: Vec<HashMap<String, String>>) -> Self {
        self.lists.insert(key.to_string(), items);
        self
    }
}

use crate::runtime::template::format_commit as format_post_render;

fn render_template(template: &str, ctx: &PromptContext) -> String {
    let mut values = serde_json::Map::new();
    for (key, value) in &ctx.text {
        values.insert(key.clone(), serde_json::json!(value));
    }
    for (key, value) in &ctx.lists {
        values.insert(key.clone(), serde_json::json!(value));
    }
    let out = crate::runtime::template::render_raw(template, &values.into())
        .expect("embedded prompt template is well-formed");
    format_post_render(&out)
}

fn family_template(family: PromptFamily) -> &'static str {
    match family {
        PromptFamily::Analysis => include_str!("prompts/analysis.md"),
        PromptFamily::Fast => include_str!("prompts/fast.md"),
        PromptFamily::Map => include_str!("prompts/map.md"),
        PromptFamily::Reduce => include_str!("prompts/reduce.md"),
        PromptFamily::Summary => include_str!("prompts/summary.md"),
        PromptFamily::SummaryRewrite => include_str!("prompts/summary-rewrite.md"),
    }
}

/// Render a prompt family. Text before `<!-- USER -->` is the system prompt
/// (not rendered); the remainder is rendered as the user prompt.
pub(crate) fn render_conventional_prompt(
    family: PromptFamily,
    ctx: &PromptContext,
) -> RenderedPrompt {
    let template = family_template(family);
    match template.split_once(USER_SEPARATOR) {
        Some((system, user)) => RenderedPrompt {
            system: system.trim().to_string(),
            user: render_template(user, ctx).trim().to_string(),
        },
        None => RenderedPrompt {
            system: String::new(),
            user: render_template(template, ctx).trim().to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
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
}
