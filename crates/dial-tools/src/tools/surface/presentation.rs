//! Portable tool-owned presentation. Surfaces choose layout, tools choose meaning.
use crate::tools::ToolOutput;
use serde_json::Value;

pub use crate::runtime::presentation::{BodyStyle, Presentation, Tone};

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// Presenters for the file/shell and controller tools. Extension definitions can override both hooks.
pub fn render_call(name: &str, args: &Value) -> Presentation {
    if name == "bash"
        && let Some(check) = args["_dial_check"].as_str()
    {
        return Presentation {
            label: "Check".into(),
            target: one_line(check),
            tone: Tone::Shell,
            body: BodyStyle::Quiet,
            ..Default::default()
        };
    }
    use BodyStyle::*;
    use Tone::*;
    let (label, tone, body) = match name {
        "read" | "artifact_read" => ("Read", Tools, Quiet),
        "grep" | "ast_search" => ("Search", Tools, Quiet),
        "glob" | "find" => ("Find", Tools, Quiet),
        "write" => ("Write", Edit, Write),
        "edit" | "ast_edit" => ("Edit", Edit, Diff),
        "bash" => ("Run", Shell, Text),
        "kernel" | "eval" => ("Run", Tools, Text),
        "task" => ("Workers", Tone::Workers, BodyStyle::Workers),
        "todo" => ("Plan", Tools, Plan),
        "ask" => ("Ask", Tools, Text),
        "web_fetch" => ("Fetch", Connect, Text),
        "web_search" => ("Web", Connect, Text),
        "github" => ("GitHub", Connect, Text),
        "browser" => ("Browse", Connect, Text),
        "lsp" => ("LSP", Tools, Text),
        "tool_search" | "tool_load" => ("Tools", Tools, Quiet),
        // Loading instructions is context, not work: one quiet line naming the skill.
        "skill" => ("Skill", Tools, Quiet),
        _ if name.starts_with("mcp__") => ("MCP", Connect, Text),
        _ => (name, Tools, Text),
    };
    let field = |key: &str| args[key].as_str().unwrap_or_default();
    let target = match name {
        "task" => args["tasks"]
            .as_array()
            .map(|tasks| plural(tasks.len(), "worker"))
            .unwrap_or_else(|| {
                if args["prompt"].is_string() {
                    "1 worker".into()
                } else {
                    ["action", "op", "query"]
                        .iter()
                        .find_map(|key| args[*key].as_str())
                        .map(one_line)
                        .unwrap_or_default()
                }
            }),
        "todo" => one_line(field("op")),
        // The question the user is asked, not the call's JSON.
        "ask" => {
            let questions = args["questions"].as_array().map_or(&[][..], Vec::as_slice);
            let first = questions
                .first()
                .and_then(|q| q["question"].as_str())
                .map(one_line)
                .unwrap_or_default();
            if questions.len() > 1 {
                format!("{first} (+{} more)", questions.len() - 1)
            } else {
                first
            }
        }
        "read" if args["paths"].is_array() => args["paths"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(one_line)
            .collect::<Vec<_>>()
            .join(", "),
        "edit" => crate::tools::edit::editing::declared_targets(args)
            .map(|paths| {
                paths
                    .into_iter()
                    .map(|path| one_line(&path))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_else(|_| {
                let path = one_line(field("path"));
                if path.is_empty() {
                    "patch".into()
                } else {
                    path
                }
            }),
        "grep" | "glob" => {
            let pattern = field("pattern");
            let path = field("path");
            let glob = field("glob");
            let mut target = pattern.to_owned();
            if !path.is_empty() {
                target.push_str(&format!(" in {path}"));
            }
            if !glob.is_empty() {
                target.push_str(&format!(" ({glob})"));
            }
            one_line(&target)
        }
        _ => [
            "command", "path", "uri", "pattern", "query", "url", "action", "op", "name",
        ]
        .iter()
        .find_map(|key| args[*key].as_str().filter(|value| !value.is_empty()))
        .map(one_line)
        .unwrap_or_else(|| {
            if args.is_null() {
                String::new()
            } else {
                one_line(&args.to_string())
            }
        }),
    };
    Presentation {
        label: label.into(),
        target,
        tone,
        body,
        ..Default::default()
    }
}

pub fn render_result(name: &str, args: &Value, output: &ToolOutput) -> Presentation {
    let mut view = render_call(name, args);
    if view.body == BodyStyle::Workers {
        let jobs = output
            .data
            .as_ref()
            .and_then(|data| data.as_array().or_else(|| data["jobs"].as_array()));
        if let Some(jobs) = jobs {
            view.target = plural(jobs.len(), "worker");
        }
    }
    if !output.is_error {
        match name {
            "read" => {
                let n = output
                    .text
                    .lines()
                    .filter(|line| {
                        let line = line.trim();
                        !(line.starts_with('[') && line.ends_with(']'))
                    })
                    .count();
                if n > 0 {
                    view.facts.push(plural(n, "line"));
                }
            }
            // The collapsed card shows the chosen answers; the model still gets the full record.
            "ask" => {
                let answers: Vec<String> = output
                    .data
                    .as_ref()
                    .and_then(|data| data["results"].as_array())
                    .into_iter()
                    .flatten()
                    .map(|result| {
                        let mut picked: Vec<String> = result["selectedOptions"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(one_line)
                            .collect();
                        if let Some(custom) = result["customInput"]
                            .as_str()
                            .filter(|s| !s.trim().is_empty())
                        {
                            picked.push(format!("\u{201c}{}\u{201d}", one_line(custom)));
                        }
                        if picked.is_empty() {
                            "no answer".to_owned()
                        } else {
                            picked.join(", ")
                        }
                    })
                    .collect();
                if !answers.is_empty() {
                    view.preview = Some(format!("Answer: {}", answers.join(" · ")));
                }
            }
            "write" => {
                if let Some(content) = args["content"].as_str() {
                    let count = plural(content.lines().count(), "line");
                    view.facts.push(if output.text.contains("created ") {
                        format!("new · {count}")
                    } else {
                        count
                    });
                }
            }
            _ => {}
        }
    }
    if let Some(code) = output
        .text
        .lines()
        .find_map(|line| {
            line.strip_prefix("[exit code: ")?
                .strip_suffix(']')?
                .parse::<i64>()
                .ok()
        })
        .filter(|code| *code != 0)
    {
        view.facts.push(format!("exit {code}"));
    }
    view
}

#[cfg(test)]
mod tests;
