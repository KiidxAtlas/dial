//! Options a model's chat template reads from `chat_template_kwargs`, found by reading the template.
//!
//! An option is a top-level variable the template tests with `is defined` (the Jinja idiom for an
//! optional caller argument), minus the variables every chat renderer supplies and the names the
//! template binds itself with `for` or `macro`. Its kind follows how the template uses it: compared
//! with string literals is a choice among them, compared with `true`/`false` or used as a bare
//! condition is a flag, anything else is free text. A `set name = literal` is reported as the
//! template's own default. This only reads text; it never renders the template.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionKind {
    Flag,
    Choice(Vec<String>),
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateOption {
    pub name: String,
    pub kind: OptionKind,
    /// The literal the template falls back to (`set name = "medium"`), when it declares one.
    pub default: Option<String>,
}

/// Variables chat renderers pass themselves; they are never user options.
const SUPPLIED: &[&str] = &[
    "messages",
    "tools",
    "custom_tools",
    "tool_choice",
    "documents",
    "add_generation_prompt",
    "continue_final_message",
    "bos_token",
    "eos_token",
    "pad_token",
    "unk_token",
];

/// The options of the chat template in a Dial-served MLX model directory. Other providers, model
/// ids that are not a local directory and models without a template have none.
pub fn detect(provider: &str, model: &str) -> Vec<TemplateOption> {
    if provider != "mlx" {
        return Vec::new();
    }
    super::catalog::chat_template(Path::new(model)).map_or_else(Vec::new, |text| parse(&text))
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    /// Operators, punctuation and the end of a `{% %}`/`{{ }}` block (`;`).
    Sym(String),
}

pub fn parse(template: &str) -> Vec<TemplateOption> {
    let tokens = tokenize(template);
    let ident = |at: usize| match tokens.get(at) {
        Some(Token::Ident(name)) => Some(name.as_str()),
        _ => None,
    };
    let sym = |at: usize| match tokens.get(at) {
        Some(Token::Sym(symbol)) => Some(symbol.as_str()),
        _ => None,
    };
    let string = |at: usize| match tokens.get(at) {
        Some(Token::Str(text)) => Some(text.clone()),
        _ => None,
    };
    // A name at `at` is a top-level variable unless it follows `.` (an attribute).
    let top = |at: usize| at == 0 || sym(at - 1) != Some(".");

    let mut bound = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for at in 0..tokens.len() {
        match ident(at) {
            Some("for") => {
                let mut next = at + 1;
                while let Some(name) = ident(next).filter(|name| *name != "in") {
                    bound.push(name.to_owned());
                    next += 1;
                    while matches!(sym(next), Some("," | "(" | ")")) {
                        next += 1;
                    }
                }
            }
            Some("macro") if sym(at + 2) == Some("(") => {
                bound.extend(ident(at + 1).map(str::to_owned));
                let mut next = at + 3;
                while sym(next) != Some(")") && next < tokens.len() {
                    if ident(next).is_some() && matches!(sym(next - 1), Some("(" | ",")) {
                        bound.extend(ident(next).map(str::to_owned));
                    }
                    next += 1;
                }
            }
            Some(name) if top(at) && ident(at + 1) == Some("is") => {
                let test = if ident(at + 2) == Some("not") {
                    at + 3
                } else {
                    at + 2
                };
                if ident(test) == Some("defined") && !names.iter().any(|known| known == name) {
                    names.push(name.to_owned());
                }
            }
            _ => {}
        }
    }
    names.retain(|name| !SUPPLIED.contains(&name.as_str()) && !bound.contains(name));

    let flag_word = |at: usize| {
        ident(at).is_some_and(|word| matches!(word, "true" | "false" | "True" | "False"))
    };
    names
        .into_iter()
        .map(|name| {
            let (mut choices, mut flag, mut default) = (Vec::<String>::new(), false, None);
            for at in (0..tokens.len()).filter(|&at| ident(at) == Some(&name) && top(at)) {
                let compared = matches!(sym(at + 1), Some("==" | "!="));
                if compared && let Some(text) = string(at + 2) {
                    choices.push(text);
                } else if at >= 2
                    && matches!(sym(at - 1), Some("==" | "!="))
                    && let Some(text) = string(at - 2)
                {
                    choices.push(text);
                }
                let tested = ident(at + 1) == Some("is");
                let test = if ident(at + 2) == Some("not") {
                    at + 3
                } else {
                    at + 2
                };
                if (compared && flag_word(at + 2))
                    || (tested && (flag_word(test) || ident(test) == Some("sameas")))
                {
                    flag = true;
                }
                let condition = at >= 1
                    && matches!(ident(at - 1), Some("if" | "elif" | "not" | "and" | "or"))
                    && (matches!(sym(at + 1), Some(";" | ")"))
                        || matches!(ident(at + 1), Some("and" | "or" | "else")));
                flag |= condition;
                if at >= 1 && ident(at - 1) == Some("set") && sym(at + 1) == Some("=") {
                    if let Some(text) = string(at + 2) {
                        choices.push(text.clone());
                        default = Some(text);
                    } else if flag_word(at + 2) {
                        flag = true;
                        default = ident(at + 2).map(str::to_lowercase);
                    }
                }
            }
            let mut unique = Vec::new();
            for choice in choices {
                if !unique.contains(&choice) {
                    unique.push(choice);
                }
            }
            let kind = match unique.len() {
                // One literal is a default or a single comparison, not a list to choose from.
                0 | 1 if flag => OptionKind::Flag,
                0 | 1 => OptionKind::Text,
                _ => OptionKind::Choice(unique),
            };
            TemplateOption {
                name,
                kind,
                default,
            }
        })
        .collect()
}

/// Identifiers, string literals and symbols inside `{% %}` and `{{ }}` blocks; text and comments
/// outside them are skipped. Each block ends with a `;` symbol.
fn tokenize(template: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find(['{']) {
        let after = &rest[open + 1..];
        let close = match after.chars().next() {
            Some('%') => "%}",
            Some('{') => "}}",
            _ => {
                rest = after;
                continue;
            }
        };
        let body = &after[1..];
        let end = body.find(close).unwrap_or(body.len());
        // `{%-`/`-%}` and `+` only control whitespace; they are not part of the expression.
        let inner = body[..end].trim().trim_matches(['-', '+']);
        tokenize_block(inner, &mut tokens);
        tokens.push(Token::Sym(";".into()));
        rest = body.get(end + close.len()..).unwrap_or("");
    }
    tokens
}

fn tokenize_block(block: &str, tokens: &mut Vec<Token>) {
    let chars: Vec<char> = block.chars().collect();
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        if c.is_whitespace() {
            at += 1;
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = at;
            while at < chars.len() && (chars[at].is_ascii_alphanumeric() || chars[at] == '_') {
                at += 1;
            }
            tokens.push(Token::Ident(chars[start..at].iter().collect()));
        } else if c == '"' || c == '\'' {
            let mut text = String::new();
            at += 1;
            while at < chars.len() && chars[at] != c {
                if chars[at] == '\\' && at + 1 < chars.len() {
                    at += 1;
                }
                text.push(chars[at]);
                at += 1;
            }
            at += 1;
            tokens.push(Token::Str(text));
        } else if c.is_ascii_digit() {
            while at < chars.len() && (chars[at].is_ascii_alphanumeric() || chars[at] == '.') {
                at += 1;
            }
            tokens.push(Token::Sym("0".into()));
        } else {
            let pair: String = chars[at..(at + 2).min(chars.len())].iter().collect();
            let symbol = if matches!(pair.as_str(), "==" | "!=" | "<=" | ">=") {
                pair
            } else {
                c.to_string()
            };
            at += symbol.chars().count();
            tokens.push(Token::Sym(symbol));
        }
    }
}

#[cfg(test)]
mod tests;
