//! `/rule <complaint>`: author one stream rule from a complaint, check it against this session's
//! assistant output, and save it to the project or user rules directory. Authored rules use
//! regex and finalized-source AST triggers; judged `question` rules are written by hand.

use crate::agent::safety::rules::{self, Catalog, State};
use crate::agent::{Agent, NullUi, make_backend};
use crate::llm::{Reply, ToolCall};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

pub const MAX_ATTEMPTS: usize = 3;
const CANDIDATE_SOURCE: &str = "rule:candidate";

const PROMPT: &str = include_str!("resources/rule_author.md");

/// A parsed model reply: the rule JSON, its generated markdown, and the fields it was built from.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: String,
    pub json: String,
    markdown: String,
    generated: Generated,
}

#[derive(Debug, Clone)]
struct Generated {
    name: String,
    description: String,
    conditions: Vec<String>,
    ast_conditions: Vec<String>,
    ast_language: Option<String>,
    scopes: Vec<String>,
    body: String,
}

/// Outcome of checking one model reply against the session.
pub enum Evaluation {
    /// The reply is not a loadable rule; the reason is fed back to the model.
    Invalid(String),
    /// A loadable rule that matches no assistant output in this session.
    Unmatched(Candidate),
    /// A loadable rule that matches at least one assistant output in this session.
    Matched(Candidate),
}

/// Where a saved rule landed, and whether the live rule catalog now includes it.
pub struct SavedRule {
    pub path: PathBuf,
    pub active: bool,
}

/// Renders the rule-author prompt. `feedback` holds failed attempts and amendments so far.
pub fn build_prompt(complaint: &str, feedback: &str, previous: Option<&str>) -> String {
    let section = if feedback.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\nFailed attempts or requested amendments so far:\n{}\n\nLatest candidate JSON:\n{}\n\nRegenerate one corrected rule. Fix the listed validation failures or user amendment. NEVER repeat failed scopes or triggers.\n",
            feedback.trim(),
            previous.unwrap_or("(none)"),
        )
    };
    crate::runtime::template::render_raw(
        PROMPT,
        &json!({"complaint":complaint.trim(), "feedback":section}),
    )
    .expect("bundled rule-author template is valid")
}

/// Pulls the rule JSON out of a reply: the first fenced block holding an object, else the first
/// balanced object that parses. Braces in surrounding prose are skipped.
pub(crate) fn extract_json(reply: &str) -> Option<String> {
    if let Some(start) = reply.find("```") {
        let mut after = &reply[start + 3..];
        if after
            .get(..4)
            .is_some_and(|tag| tag.eq_ignore_ascii_case("json"))
        {
            after = &after[4..];
        }
        if let Some(end) = after.find("```") {
            let inner = after[..end].trim();
            if inner.starts_with('{') {
                return Some(inner.to_string());
            }
        }
    }
    let mut first = None;
    for (start, _) in reply.match_indices('{') {
        let Some(len) = balanced_object_len(&reply[start..]) else {
            continue;
        };
        let object = &reply[start..start + len];
        if serde_json::from_str::<Value>(object).is_ok_and(|value| value.is_object()) {
            return Some(object.to_string());
        }
        first.get_or_insert_with(|| object.to_string());
    }
    first
}

/// Byte length of the balanced `{...}` object at the start of `text`, skipping braces in strings.
fn balanced_object_len(text: &str) -> Option<usize> {
    let (mut depth, mut in_string, mut escaped) = (0i32, false, false);
    for (offset, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Name rule: lowercase, quotes dropped, anything outside `[a-z0-9_-]` becomes `-`.
pub(crate) fn sanitize_name(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, '"' | '\'' | '`'))
    {
        let mapped = if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-' {
            ch
        } else {
            '-'
        };
        if mapped == '-' && out.ends_with('-') {
            continue;
        }
        out.push(mapped);
    }
    out.trim_matches('-').to_string()
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn text_field(value: Option<&Value>) -> Option<String> {
    value?.as_str().and_then(non_empty)
}

fn string_list(value: Option<&Value>, field: &str) -> Result<Vec<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(non_empty(text).into_iter().collect()),
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                let text = item
                    .as_str()
                    .ok_or_else(|| format!("`{field}` entries must be strings"))?;
                out.extend(non_empty(text));
            }
            Ok(out)
        }
        Some(_) => Err(format!("`{field}` must be a string or an array of strings")),
    }
}

/// Validates the generated JSON fields. Accepts the `desc` and `cond` aliases.
fn parse_generated(raw: &str) -> Result<Generated, String> {
    let value: Value =
        serde_json::from_str(raw).map_err(|error| format!("invalid JSON: {error}"))?;
    let Some(object) = value.as_object() else {
        return Err("Missing generated rule JSON object".into());
    };
    let raw_name = text_field(object.get("name"))
        .ok_or("Generated rule JSON must include a non-empty name")?;
    let name = sanitize_name(&raw_name);
    if !name.chars().any(|c| c.is_ascii_alphanumeric()) {
        return Err("Rule name must contain at least one letter or digit".into());
    }
    let description = text_field(object.get("description").or_else(|| object.get("desc")))
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .ok_or("Generated rule JSON must include a non-empty description")?;
    if object.get("question").is_some_and(|value| !value.is_null()) {
        return Err("question triggers are not supported; use condition or astCondition".into());
    }
    let conditions = string_list(
        object.get("condition").or_else(|| object.get("cond")),
        "condition",
    )?;
    let ast_conditions = string_list(object.get("astCondition"), "astCondition")?;
    let ast_language = text_field(object.get("astLanguage"));
    if object.contains_key("astLanguage") && ast_language.is_none() {
        return Err("astLanguage must be a nonempty language name".into());
    }
    if conditions.is_empty() && ast_conditions.is_empty() {
        return Err("Generated rule JSON must include condition or astCondition".into());
    }
    let scopes = string_list(object.get("scope"), "scope")?;
    if scopes.is_empty() {
        return Err("Generated rule JSON must include at least one scope".into());
    }
    let body = text_field(object.get("body"))
        .ok_or("Generated rule JSON must include a non-empty body")?;
    Ok(Generated {
        name,
        description,
        conditions,
        ast_conditions,
        ast_language,
        scopes,
        body,
    })
}

fn quote(text: &str) -> String {
    json!(text).to_string()
}

/// One value stays a JSON string; several become a flow sequence of JSON strings.
fn flow_list(items: &[String]) -> String {
    if let [one] = items {
        return quote(one);
    }
    format!(
        "[{}]",
        items
            .iter()
            .map(|item| quote(item))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn assemble(generated: &Generated, conditions: &[String]) -> String {
    let mut front = format!(
        "---\nname: {}\ndescription: {}\n",
        quote(&generated.name),
        quote(&generated.description)
    );
    if !conditions.is_empty() {
        front.push_str(&format!("condition: {}\n", flow_list(conditions)));
    }
    if !generated.ast_conditions.is_empty() {
        front.push_str(&format!(
            "astCondition: {}\n",
            flow_list(&generated.ast_conditions)
        ));
    }
    if let Some(language) = &generated.ast_language {
        front.push_str(&format!("astLanguage: {}\n", quote(language)));
    }
    format!(
        "{front}scope: {}\n---\n\n{}\n",
        flow_list(&generated.scopes),
        generated.body
    )
}

/// One level of JSON-style escape repair: `\\\\` written by the model becomes `\\`.
fn unescape_once(pattern: &str) -> String {
    pattern.replace("\\\\", "\\")
}

fn candidate_from_reply(reply: &str) -> Result<Candidate, String> {
    let json = extract_json(reply).ok_or("Missing generated rule JSON object")?;
    let generated = parse_generated(&json)?;
    let markdown = assemble(&generated, &generated.conditions);
    let first = match rules::parse_candidate(&markdown, CANDIDATE_SOURCE) {
        Ok(_) => {
            return Ok(Candidate {
                name: generated.name.clone(),
                json,
                markdown,
                generated,
            });
        }
        Err(error) => error,
    };
    let repaired: Vec<String> = generated
        .conditions
        .iter()
        .map(|c| unescape_once(c))
        .collect();
    let repaired_markdown = assemble(&generated, &repaired);
    if repaired == generated.conditions
        || rules::parse_candidate(&repaired_markdown, CANDIDATE_SOURCE).is_err()
    {
        return Err(first);
    }
    Ok(Candidate {
        name: generated.name.clone(),
        json,
        markdown: repaired_markdown,
        generated,
    })
}

/// One stored assistant message as the reply the rule monitor would have seen.
fn history_reply(message: &Value) -> Reply {
    let tool_calls = message["tool_calls"]
        .as_array()
        .map(|calls| {
            calls
                .iter()
                .map(|call| {
                    let parsed = call["function"]["arguments"]
                        .as_str()
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                        .filter(Value::is_object);
                    ToolCall {
                        id: call["id"].as_str().unwrap_or_default().to_string(),
                        name: call["function"]["name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        malformed_arguments: parsed.is_none(),
                        arguments: parsed.unwrap_or(Value::Null),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    Reply {
        text: message["content"].as_str().unwrap_or_default().to_string(),
        tool_calls,
        ..Default::default()
    }
}

impl Agent<'_> {
    /// One ephemeral turn over the session plus the `/rule` prompt. No tools; nothing is appended.
    pub fn rule_generate(&mut self, prompt: &str) -> Result<String> {
        let config = self.config;
        let mut main = match self.backend.take() {
            Some(backend) => backend,
            None => make_backend(config)?,
        };
        let result = (|| {
            let history = self.session.messages();
            let mut request = self.projected_request(&history, None)?;
            request.tools.clear();
            request
                .messages
                .push(json!({"role": "user", "content": prompt}));
            let mut ui = NullUi;
            self.role_chat(
                &mut main,
                "rule",
                crate::llm::usage::Purpose::Auxiliary,
                &request,
                &mut ui,
            )
        })();
        self.backend = Some(main);
        result
    }

    /// Parses `reply` as a rule and checks whether it fires on this session's assistant output.
    /// Retries once with unescaped regex conditions when the first form does not match.
    pub fn rule_evaluate(&self, reply: &str) -> Evaluation {
        let mut candidate = match candidate_from_reply(reply) {
            Ok(candidate) => candidate,
            Err(reason) => return Evaluation::Invalid(reason),
        };
        match self.rule_matches_history(&candidate.markdown) {
            Ok(true) => return Evaluation::Matched(candidate),
            Ok(false) => {}
            Err(reason) => return Evaluation::Invalid(reason),
        }
        let repaired: Vec<String> = candidate
            .generated
            .conditions
            .iter()
            .map(|c| unescape_once(c))
            .collect();
        if repaired != candidate.generated.conditions {
            let markdown = assemble(&candidate.generated, &repaired);
            if let Ok(true) = self.rule_matches_history(&markdown) {
                candidate.markdown = markdown;
                return Evaluation::Matched(candidate);
            }
        }
        Evaluation::Unmatched(candidate)
    }

    fn rule_matches_history(&self, markdown: &str) -> Result<bool, String> {
        let catalog = rules::parse_candidate(markdown, CANDIDATE_SOURCE)?;
        let root = self.workspace.root().to_path_buf();
        let state = State::default();
        for message in self.session.messages() {
            if message["role"].as_str() != Some("assistant") {
                continue;
            }
            let reply = history_reply(&message);
            let mut monitor = catalog.monitor(&root, &state, "rule");
            if monitor.completed(&reply).is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Target file for a rule named `name`: `<project>/.dial/rules` or the user rules directory.
    pub fn rule_target(&self, name: &str, global: bool) -> Result<PathBuf> {
        let dir = if global {
            self.layout
                .as_ref()
                .context("no user directory is configured for global rules")?
                .user_dir
                .join("rules")
        } else {
            self.workspace.root().join(".dial").join("rules")
        };
        Ok(dir.join(format!("{name}.md")))
    }

    /// Writes the rule and refreshes the live catalog. A file the loader refuses is removed.
    pub fn rule_save(
        &mut self,
        candidate: &Candidate,
        global: bool,
        overwrite: bool,
    ) -> Result<SavedRule> {
        let path = self.rule_target(&candidate.name, global)?;
        if path.exists() && !overwrite {
            bail!("{} already exists", path.display());
        }
        // A refused overwrite restores the rule it replaced rather than deleting it.
        let replaced = if overwrite && path.exists() {
            Some(std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?)
        } else {
            None
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        crate::platform::atomic::write_replacing(&path, candidate.markdown.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        let active = !self.hermetic && self.config.portable_rules.enabled;
        if active && let Some(layout) = self.layout.as_ref() {
            let root = self.workspace.root().to_path_buf();
            let catalog = Catalog::discover(layout, &root, &self.config.portable_rules);
            let file = format!("{}.md", candidate.name);
            if let Some(warning) = catalog.warnings.iter().find(|w| w.contains(&file)) {
                let rollback = match replaced {
                    Some(bytes) => crate::platform::atomic::write_replacing(&path, &bytes),
                    None => std::fs::remove_file(&path),
                };
                if let Err(error) = rollback {
                    bail!(
                        "the rule loader refused the saved rule: {warning}; restoring {} also failed ({error}), so the rejected rule remains on disk",
                        path.display()
                    );
                }
                bail!("the rule loader refused the saved rule: {warning}");
            }
            self.context_rule_catalog = Some(Arc::new(catalog));
        }
        Ok(SavedRule { path, active })
    }
}

#[cfg(test)]
mod tests;

/// Fake-server tests: one scripted turn builds history, then `/rule` generation answers next.
#[cfg(test)]
mod fake_server_tests;
