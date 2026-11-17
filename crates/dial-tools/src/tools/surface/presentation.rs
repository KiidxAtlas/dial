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

// Desktop calls may contain typed secrets or script source. Describe only the operation and
// explicit window selector, never arbitrary call arguments or evaluated return objects.
fn desktop_target(args: &Value) -> String {
    match args["action"].as_str() {
        Some("run") => "script".into(),
        Some("call") => {
            let Some(chain) = args["chain"].as_array() else {
                return "call".into();
            };
            let mut parts = Vec::new();
            for call in chain.iter().take(4) {
                let Some(method) = call["method"].as_str() else {
                    continue;
                };
                let mut part = desktop_fragment(method);
                if method == "window" {
                    let selector = &call["args"][0];
                    if let Some(target) = selector
                        .as_str()
                        .or_else(|| selector["app"].as_str())
                        .or_else(|| selector["title"].as_str())
                    {
                        part.push(' ');
                        part.push_str(&desktop_fragment(target));
                    }
                }
                parts.push(part);
            }
            if chain.len() > 4 {
                parts.push(format!("+{} steps", chain.len() - 4));
            }
            if parts.is_empty() {
                "call".into()
            } else {
                parts.join(" · ")
            }
        }
        Some(action) => desktop_fragment(action),
        None => "desktop operation".into(),
    }
}

fn desktop_fragment(value: &str) -> String {
    one_line(&crate::runtime::presentation::truncate(value, 160))
}

fn desktop_result(view: &mut Presentation, args: &Value, output: &ToolOutput) {
    let Some(data) = &output.data else { return };
    if let Some(shots) = data["screenshots"]
        .as_array()
        .filter(|shots| !shots.is_empty())
    {
        view.facts.push(plural(shots.len(), "screenshot"));
        view.preview = Some("Screenshot available · expand for capture details".into());
    }
    // Only a direct windows() result has the window-list contract. A script can return any array.
    let lists_windows = args["action"] == "call"
        && args["chain"]
            .as_array()
            .and_then(|chain| chain.last())
            .is_some_and(|call| call["method"] == "windows");
    if lists_windows && let Some(windows) = data["value"].as_array() {
        view.facts.push(plural(windows.len(), "window"));
        let mut rows: Vec<String> = windows
            .iter()
            .take(3)
            .map(|window| {
                let app = window["app"].as_str().unwrap_or("Window");
                let title = window["title"].as_str().unwrap_or_default();
                if title.is_empty() {
                    desktop_fragment(app)
                } else {
                    format!("{} · {}", desktop_fragment(app), desktop_fragment(title))
                }
            })
            .collect();
        if windows.len() > 3 {
            rows.push(format!(
                "{} more windows · expand for details",
                windows.len() - 3
            ));
        }
        view.preview = Some(if rows.is_empty() {
            "No windows returned".into()
        } else {
            rows.join("\n")
        });
    }
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
        "read" => ("Read", Tools, Quiet),
        "grep" | "ast_search" => ("Search", Tools, Quiet),
        "glob" | "find" => ("Find", Tools, Quiet),
        "write" => ("Write", Edit, Write),
        "edit" | "ast_edit" => ("Edit", Edit, Diff),
        "bash" => ("Run", Shell, Text),
        "eval" => ("Eval", Tools, Text),
        "task" => ("Workers", Tone::Workers, BodyStyle::Workers),
        "todo" => ("Plan", Tools, Plan),
        "ask" => ("Ask", Tools, Text),
        "web_fetch" => ("Fetch", Connect, Text),
        "web_search" => ("Web", Connect, Text),
        "github" => ("GitHub", Connect, Text),
        "browser" => ("Browse", Connect, Text),
        "computer" => ("Desktop", Tools, Text),
        "lsp" => ("LSP", Tools, Text),
        "tool_search" | "tool_load" => ("Tools", Tools, Quiet),
        // Loading instructions is context, not work: one quiet line naming the skill.
        "skill" => ("Skill", Tools, Quiet),
        // Notes between agents; the terminal draws a sent note as a quoted IRC line.
        "message" => ("IRC", Connect, Text),
        _ if name.starts_with("mcp__") => ("MCP", Connect, Text),
        _ => (name, Tools, Text),
    };
    let field = |key: &str| args[key].as_str().unwrap_or_default();
    let target = match name {
        "computer" => desktop_target(args),
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
        "message" => one_line(field("to")),
        "skill" => {
            let name = one_line(field("name"));
            let file = one_line(field("file"));
            if file.is_empty() {
                name
            } else {
                format!("{name} · {file}")
            }
        }
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
            "computer" => desktop_result(&mut view, args, output),
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
            // A search card says what it found, as a read card says how much it read.
            "grep" => {
                let hits = output
                    .text
                    .lines()
                    .filter(|line| {
                        let mut parts = line.splitn(3, ':');
                        let (_, number) = (parts.next(), parts.next());
                        parts.next().is_some()
                            && number.is_some_and(|n| {
                                !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
                            })
                    })
                    .count();
                if hits > 0 {
                    view.facts.push(if hits == 1 {
                        "1 match".to_owned()
                    } else {
                        format!("{hits} matches")
                    });
                } else if output.text.trim_start().starts_with("no matches") {
                    view.facts.push("no matches".into());
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
        // Errors a live language server reported after an edit: a count on the card, the lines in
        // the result (and in `dial watch`).
        if let Some((count, _)) = crate::tools::code::lsp::new_errors_in(&output.text) {
            view.facts.push(plural(count, "new error"));
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
