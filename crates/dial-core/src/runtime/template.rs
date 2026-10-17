//! System-prompt templates: a Handlebars-compatible renderer covering the subset of
//! Handlebars syntax the prompts use (standalone-line stripping, `{{x}}` / `{{{x}}}` / `{{&x}}`,
//! `#if` / `#unless` / `#each` / `#with` / `#when` / `#ifAny` / `#ifAll` / `#has` / `#list`,
//! inverse blocks, `else` chains, `@data` and `../` paths, sub-expressions and hash arguments),
//! the prompt helpers registered by this module, and the post-render `format()`
//! normalization. Output is never HTML-escaped.

use anyhow::{Context as _, Result, anyhow, bail};
use parking_lot::RwLock;
use serde_json::{Number, Value, json};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::Path as FsPath;
use std::sync::Arc;

const BLOCK_HELPERS: &[&str] = &[
    "if",
    "unless",
    "each",
    "with",
    "when",
    "ifAny",
    "ifAll",
    "has",
    "list",
    "table",
    "codeblock",
    "xml",
];
const VALUE_HELPERS: &[&str] = &[
    "join",
    "len",
    "add",
    "sub",
    "not",
    "default",
    "pluralize",
    "includes",
    "lookup",
    "jsonStringify",
    "escapeXml",
];

/// A compiled system-prompt template.
#[derive(Debug)]
pub struct Template {
    nodes: Vec<Node>,
}

#[derive(Debug)]
enum Node {
    Text(String),
    Output(Expr),
    Partial(String),
    Block {
        head: Expr,
        inverted: bool,
        body: Vec<Node>,
        otherwise: Vec<Node>,
    },
}

#[derive(Debug)]
enum Expr {
    Value(Arg),
    Call(Call),
}

#[derive(Debug)]
struct Call {
    name: String,
    args: Vec<Arg>,
    hash: Vec<(String, Arg)>,
}

#[derive(Debug)]
enum Arg {
    Lit(Value),
    Path(Path),
    Sub(Box<Expr>),
}

#[derive(Debug, Default)]
struct Path {
    up: usize,
    root: bool,
    data: Option<String>,
    segs: Vec<String>,
}

#[derive(Debug)]
enum Tok {
    Text(String),
    Mustache(String),
    Raw(String),
    Partial(String),
    Open { inverted: bool, expr: String },
    Close(String),
    Else(String),
}

#[derive(Debug)]
enum Stop {
    End,
    Close(String),
    Else(String),
}

/// Partials registered with [`register_partial`]; `{{> name}}` resolves here.
static PARTIALS: RwLock<BTreeMap<String, Arc<Template>>> = RwLock::new(BTreeMap::new());

/// Register a partial for `{{> name}}`. The source is parsed now,
/// so a malformed partial fails at registration rather than mid-render.
#[allow(dead_code)] // No production caller registers partials.
pub fn register_partial(name: &str, source: &str) -> Result<()> {
    let template =
        Template::parse(source).map_err(|error| anyhow!("Invalid partial {name}: {error}"))?;
    PARTIALS.write().insert(name.to_owned(), Arc::new(template));
    Ok(())
}

fn partial_template(name: &str) -> Option<Arc<Template>> {
    PARTIALS.read().get(name).cloned()
}

/// Read a template file. Blank files are rejected.
pub fn load_file(path: &FsPath) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading system prompt template {}", path.display()))?;
    if text.trim().is_empty() {
        bail!(
            "System prompt template must not be empty: {}",
            path.display()
        );
    }
    Ok(text)
}

/// Compile template source. Blank source and syntax errors are rejected up front.
pub fn compile(source: &str) -> Result<Template> {
    if source.trim().is_empty() {
        bail!("System prompt template must not be empty");
    }
    Template::parse(source).map_err(|error| anyhow!("Invalid system prompt template: {error}"))
}

/// Render a compiled template against `data` and normalize the output.
pub fn render_system_prompt(template: &Template, data: &Value) -> Result<String> {
    let rendered = template
        .render(data)
        .map_err(|error| anyhow!("Invalid system prompt template: {error}"))?;
    Ok(format_system(&rendered))
}

/// Render without whitespace normalization; callers select their explicit formatting policy.
pub fn render_raw(source: &str, data: &Value) -> Result<String> {
    Template::parse(source)
        .map_err(anyhow::Error::msg)?
        .render(data)
        .map_err(anyhow::Error::msg)
}

impl Template {
    fn parse(source: &str) -> Result<Self, String> {
        let tokens = tokenize(&strip_standalone(source))?;
        let mut pos = 0;
        let (nodes, stop) = parse_seq(&tokens, &mut pos)?;
        match stop {
            Stop::End => Ok(Self { nodes }),
            Stop::Close(name) => Err(mismatched(&name)),
            Stop::Else(_) => Err("Parse error: unexpected else".to_owned()),
        }
    }

    fn render(&self, data: &Value) -> Result<String, String> {
        let mut renderer = Renderer {
            frames: vec![Frame::new(data.clone())],
        };
        let mut out = String::new();
        renderer.render_nodes(&self.nodes, &mut out)?;
        Ok(out)
    }
}

// ---------------------------------------------------------------- standalone lines and tokens

fn line_body(line: &str) -> &str {
    match line.strip_suffix('\n') {
        Some(rest) => rest.strip_suffix('\r').unwrap_or(rest),
        None => line,
    }
}

/// `{{#x}}`, `{{/x}}`, `{{^x}}`, `{{else ...}}` or `{{! ...}}` alone on a line (no nested braces).
fn is_standalone_tag(body: &str) -> bool {
    let Some(inner) = body
        .trim()
        .strip_prefix("{{")
        .and_then(|rest| rest.strip_suffix("}}"))
    else {
        return false;
    };
    if inner.contains('{') || inner.contains('}') {
        return false;
    }
    let sigil = inner.starts_with(['#', '/', '^', '!']);
    let keyword = inner.strip_prefix("else").is_some_and(|rest| {
        rest.chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
    });
    sigil || keyword
}

/// Block-tag-only lines lose their indentation and newline,
/// and multi-line `{{!-- --}}` comments that end on their own line are removed.
fn strip_standalone(source: &str) -> String {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let mut out = String::with_capacity(source.len());
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let body = line_body(line);
        if body.trim_start().starts_with("{{!--") && !body.contains("--}}") {
            let closing = lines[index + 1..]
                .iter()
                .enumerate()
                .find_map(|(offset, next)| {
                    let text = line_body(next);
                    text.find("--}}")
                        .map(|at| (offset, text[at + 4..].trim().is_empty()))
                });
            match closing {
                Some((offset, true)) => index += offset + 2,
                _ => {
                    out.push_str(line);
                    index += 1;
                }
            }
            continue;
        }
        if is_standalone_tag(body) {
            out.push_str(body.trim());
        } else {
            out.push_str(line);
        }
        index += 1;
    }
    out
}

fn classify(inner: &str) -> Tok {
    if let Some(rest) = inner.strip_prefix('#') {
        return Tok::Open {
            inverted: false,
            expr: rest.trim().to_owned(),
        };
    }
    if let Some(rest) = inner.strip_prefix('^') {
        return Tok::Open {
            inverted: true,
            expr: rest.trim().to_owned(),
        };
    }
    if let Some(rest) = inner.strip_prefix('/') {
        return Tok::Close(rest.trim().to_owned());
    }
    if let Some(rest) = inner.strip_prefix('>') {
        return Tok::Partial(rest.trim().to_owned());
    }
    if inner == "else" {
        return Tok::Else(String::new());
    }
    if let Some(rest) = inner
        .strip_prefix("else")
        .filter(|rest| rest.starts_with(char::is_whitespace))
    {
        return Tok::Else(rest.trim().to_owned());
    }
    Tok::Mustache(inner.to_owned())
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let unclosed_comment = || "Parse error: unclosed comment".to_owned();
    let unclosed_expression = || "Parse error: unclosed template expression".to_owned();
    let mut tokens = Vec::new();
    let mut pos = 0;
    while let Some(rel) = src[pos..].find("{{") {
        let open = pos + rel;
        if open > pos {
            tokens.push(Tok::Text(src[pos..open].to_owned()));
        }
        let start = open + 2;
        let rest = &src[start..];
        if let Some(after) = rest.strip_prefix("!--") {
            let end = after.find("--}}").ok_or_else(unclosed_comment)?;
            pos = start + 3 + end + 4;
        } else if rest.starts_with('!') {
            let end = rest.find("}}").ok_or_else(unclosed_comment)?;
            pos = start + end + 2;
        } else if let Some(after) = rest.strip_prefix('{') {
            let end = after.find("}}}").ok_or_else(unclosed_expression)?;
            tokens.push(Tok::Raw(after[..end].trim().to_owned()));
            pos = start + 1 + end + 3;
        } else if let Some(after) = rest.strip_prefix('&') {
            let end = after.find("}}").ok_or_else(unclosed_expression)?;
            tokens.push(Tok::Raw(after[..end].trim().to_owned()));
            pos = start + 1 + end + 2;
        } else {
            let end = rest.find("}}").ok_or_else(unclosed_expression)?;
            tokens.push(classify(rest[..end].trim()));
            pos = start + end + 2;
        }
    }
    if pos < src.len() {
        tokens.push(Tok::Text(src[pos..].to_owned()));
    }
    Ok(tokens)
}

// ---------------------------------------------------------------- parser

fn mismatched(name: &str) -> String {
    format!("Parse error: mismatched {}{}{}", "{{/", name, "}}")
}

fn expect_close(name: &str, owner: &str) -> Result<(), String> {
    if name == owner {
        Ok(())
    } else {
        Err(mismatched(name))
    }
}

fn parse_seq(tokens: &[Tok], pos: &mut usize) -> Result<(Vec<Node>, Stop), String> {
    let mut nodes = Vec::new();
    while let Some(token) = tokens.get(*pos) {
        *pos += 1;
        match token {
            Tok::Text(text) => nodes.push(Node::Text(text.clone())),
            Tok::Mustache(expr) | Tok::Raw(expr) => nodes.push(Node::Output(parse_expr(expr)?)),
            Tok::Partial(name) => nodes.push(Node::Partial(name.clone())),
            Tok::Open { inverted, expr } => {
                nodes.push(parse_block(tokens, pos, *inverted, expr, None)?)
            }
            Tok::Close(name) => return Ok((nodes, Stop::Close(name.clone()))),
            Tok::Else(rest) => return Ok((nodes, Stop::Else(rest.clone()))),
        }
    }
    Ok((nodes, Stop::End))
}

/// Parse a block body. `owner` is the name a closing tag must match; chained `{{else x}}`
/// blocks pass the outer block's name so that a single `{{/x}}` closes the whole chain.
fn parse_block(
    tokens: &[Tok],
    pos: &mut usize,
    inverted: bool,
    expr: &str,
    owner: Option<&str>,
) -> Result<Node, String> {
    let head = parse_expr(expr)?;
    let owner = match owner {
        Some(name) => name.to_owned(),
        None => expr
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned(),
    };
    let unclosed = || format!("Parse error: unclosed block {owner}");
    let (body, stop) = parse_seq(tokens, pos)?;
    let otherwise = match stop {
        Stop::Close(name) => {
            expect_close(&name, &owner)?;
            Vec::new()
        }
        Stop::Else(rest) if rest.is_empty() => {
            let (inverse, stop) = parse_seq(tokens, pos)?;
            match stop {
                Stop::Close(name) => {
                    expect_close(&name, &owner)?;
                    inverse
                }
                Stop::Else(_) => return Err("Parse error: unexpected else".to_owned()),
                Stop::End => return Err(unclosed()),
            }
        }
        Stop::Else(rest) => vec![parse_block(
            tokens,
            pos,
            false,
            &rest,
            Some(owner.as_str()),
        )?],
        Stop::End => return Err(unclosed()),
    };
    Ok(Node::Block {
        head,
        inverted,
        body,
        otherwise,
    })
}

fn is_name(src: &str) -> bool {
    !src.is_empty()
        && src
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// Split an expression into whitespace-separated tokens, keeping quoted strings and
/// parenthesized sub-expressions intact.
fn split_tokens(src: &str) -> Result<Vec<&str>, String> {
    let mut out = Vec::new();
    let mut start = None;
    let mut quote: Option<char> = None;
    let mut depth = 0i32;
    for (index, c) in src.char_indices() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        if c.is_whitespace() && depth == 0 {
            if let Some(from) = start.take() {
                out.push(&src[from..index]);
            }
            continue;
        }
        if start.is_none() {
            start = Some(index);
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err("Parse error: unbalanced parenthesis".to_owned());
                }
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return Err("Parse error: unclosed string".to_owned());
    }
    if depth != 0 {
        return Err("Parse error: unbalanced parenthesis".to_owned());
    }
    if let Some(from) = start {
        out.push(&src[from..]);
    }
    Ok(out)
}

fn hash_split(token: &str) -> Option<(&str, &str)> {
    if token.starts_with(['"', '\'', '(']) {
        return None;
    }
    let (key, value) = token.split_once('=')?;
    is_name(key).then_some((key, value))
}

fn parse_arg(token: &str) -> Result<Arg, String> {
    if token.starts_with('(') {
        let inner = token
            .strip_prefix('(')
            .and_then(|rest| rest.strip_suffix(')'))
            .ok_or_else(|| "Parse error: unbalanced parenthesis".to_owned())?;
        return Ok(Arg::Sub(Box::new(parse_expr(inner)?)));
    }
    if let Some(quote) = token.chars().next().filter(|c| *c == '"' || *c == '\'') {
        return match token.strip_suffix(quote) {
            Some(inner) if token.len() >= 2 => Ok(Arg::Lit(Value::String(inner[1..].to_owned()))),
            _ => Err("Parse error: unclosed string".to_owned()),
        };
    }
    if let Some(number) = token.parse::<f64>().ok().and_then(Number::from_f64) {
        return Ok(Arg::Lit(Value::Number(number)));
    }
    Ok(match token {
        "true" => Arg::Lit(Value::Bool(true)),
        "false" => Arg::Lit(Value::Bool(false)),
        "null" | "undefined" => Arg::Lit(Value::Null),
        _ => Arg::Path(Path::parse(token)),
    })
}

fn parse_expr(src: &str) -> Result<Expr, String> {
    let tokens = split_tokens(src)?;
    let Some((first, rest)) = tokens.split_first() else {
        return Err("Parse error: empty expression".to_owned());
    };
    if rest.is_empty() && hash_split(first).is_none() {
        let arg = parse_arg(first)?;
        return Ok(match arg {
            Arg::Path(_) if is_name(first) => Expr::Call(Call {
                name: (*first).to_owned(),
                args: Vec::new(),
                hash: Vec::new(),
            }),
            other => Expr::Value(other),
        });
    }
    if hash_split(first).is_some() || !is_name(first) {
        return Err(format!("Parse error: invalid helper name {first}"));
    }
    let mut args = Vec::new();
    let mut hash = Vec::new();
    for token in rest {
        match hash_split(token) {
            Some((key, value)) => hash.push((key.to_owned(), parse_arg(value)?)),
            None => {
                if !hash.is_empty() {
                    return Err("Parse error: positional argument after hash argument".to_owned());
                }
                args.push(parse_arg(token)?);
            }
        }
    }
    Ok(Expr::Call(Call {
        name: (*first).to_owned(),
        args,
        hash,
    }))
}

impl Path {
    fn parse(src: &str) -> Self {
        let mut path = Self::default();
        if let Some(rest) = src.strip_prefix('@') {
            let (name, tail) = rest.split_once('.').unwrap_or((rest, ""));
            if name == "root" {
                path.root = true;
            } else {
                path.data = Some(name.to_owned());
            }
            path.segs = split_segments(tail);
            return path;
        }
        let mut rest = src;
        while let Some(stripped) = rest.strip_prefix("../") {
            path.up += 1;
            rest = stripped;
        }
        let rest = rest.strip_prefix("./").unwrap_or(rest);
        path.segs = split_segments(rest);
        path
    }
}

fn split_segments(src: &str) -> Vec<String> {
    src.split('.')
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.strip_prefix('[')
                .and_then(|x| x.strip_suffix(']'))
                .unwrap_or(part)
        })
        .filter(|part| *part != "this")
        .map(str::to_owned)
        .collect()
}

// ---------------------------------------------------------------- values and helpers

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Handlebars' `isEmpty`: missing, false, empty string and empty array (but not 0).
fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// `if` truthiness: JS truthiness, empty arrays are falsy, and `0` only when `includeZero`.
fn if_truthy(value: &Value, include_zero: bool) -> bool {
    if matches!(value, Value::Number(number) if number.as_f64() == Some(0.0)) {
        return include_zero;
    }
    js_truthy(value) && !matches!(value, Value::Array(items) if items.is_empty())
}

fn fmt_num(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_owned()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else if f == 0.0 {
        "0".to_owned()
    } else if f.fract() == 0.0 && f.abs() < 1e21 {
        format!("{f:.0}")
    } else {
        format!("{f}")
    }
}

/// JS `String(value)` as used for `{{x}}` output: missing values print as empty.
fn stringify(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => fmt_num(number.as_f64().unwrap_or(f64::NAN)),
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(stringify).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

fn unescape_separator(separator: &str) -> String {
    separator.replace("\\n", "\n").replace("\\t", "\t")
}

/// JS `Number.parseInt(text, 10)`: optional leading whitespace and sign, then decimal digits.
fn js_parse_int(text: &str) -> Option<f64> {
    let text = text.trim_start();
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, text.strip_prefix('+').unwrap_or(text)),
    };
    let end = digits.bytes().take_while(u8::is_ascii_digit).count();
    if end == 0 {
        return None;
    }
    digits[..end]
        .parse::<f64>()
        .ok()
        .map(|number| sign * number)
}

/// Escapes `&`, `<`, `>` and `"` in that order; null renders empty.
fn escape_xml(value: &Value) -> String {
    if value.is_null() {
        return String::new();
    }
    stringify(value)
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn hash_value<'a>(hash: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    hash.iter()
        .find(|(name, _)| name.as_str() == key)
        .map(|(_, value)| value)
}

fn parse_js_number(text: &str) -> f64 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0.0
    } else {
        trimmed.parse::<f64>().unwrap_or(f64::NAN)
    }
}

fn to_num(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(flag) => f64::from(u8::from(*flag)),
        Value::Number(number) => number.as_f64().unwrap_or(f64::NAN),
        Value::String(text) => parse_js_number(text),
        Value::Array(_) | Value::Object(_) => parse_js_number(&stringify(value)),
    }
}

fn num_value(f: f64) -> Value {
    if f.is_nan() {
        Value::String("NaN".to_owned())
    } else if f.is_infinite() {
        Value::String(fmt_num(f))
    } else {
        Number::from_f64(f).map_or(Value::Null, Value::Number)
    }
}

fn nullish_zero(value: &Value) -> Value {
    if value.is_null() {
        Value::from(0)
    } else {
        value.clone()
    }
}

/// `(a ?? 0) + (b ?? 0)`: string concatenation when either side is a string.
fn add(a: &Value, b: &Value) -> Value {
    let (a, b) = (nullish_zero(a), nullish_zero(b));
    if matches!(a, Value::String(_)) || matches!(b, Value::String(_)) {
        return Value::String(format!("{}{}", stringify(&a), stringify(&b)));
    }
    num_value(to_num(&a) + to_num(&b))
}

fn strict_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(_) | Value::Object(_), _) | (_, Value::Array(_) | Value::Object(_)) => false,
        _ => a == b,
    }
}

fn compare(lhs: &Value, op: &str, rhs: &Value) -> Option<bool> {
    match op {
        "==" | "===" => Some(strict_eq(lhs, rhs)),
        "!=" | "!==" => Some(!strict_eq(lhs, rhs)),
        ">" | "<" | ">=" | "<=" => {
            let ordering: Option<Ordering> = match (lhs, rhs) {
                (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
                _ => to_num(lhs).partial_cmp(&to_num(rhs)),
            };
            Some(match op {
                ">" => ordering == Some(Ordering::Greater),
                "<" => ordering == Some(Ordering::Less),
                ">=" => matches!(ordering, Some(Ordering::Greater | Ordering::Equal)),
                _ => matches!(ordering, Some(Ordering::Less | Ordering::Equal)),
            })
        }
        _ => None,
    }
}

fn includes(collection: &Value, item: &Value) -> bool {
    match collection {
        Value::Array(items) => items.iter().any(|value| strict_eq(value, item)),
        _ => false,
    }
}

fn collection_has(collection: &Value, item: &Value) -> bool {
    match collection {
        Value::Array(items) => items.iter().any(|value| strict_eq(value, item)),
        Value::Object(map) => match item {
            Value::String(_) | Value::Number(_) => map.contains_key(&stringify(item)),
            _ => false,
        },
        _ => false,
    }
}

/// Property access used by paths and `lookup`: objects by key, arrays by index or `length`.
fn child(value: &Value, key: &str) -> Value {
    match value {
        Value::Object(map) => map.get(key).cloned().unwrap_or(Value::Null),
        Value::Array(items) if key == "length" => Value::from(items.len()),
        Value::Array(items) => key
            .parse::<usize>()
            .ok()
            .and_then(|index| items.get(index))
            .cloned()
            .unwrap_or(Value::Null),
        Value::String(text) if key == "length" => Value::from(text.encode_utf16().count()),
        _ => Value::Null,
    }
}

fn value_helper(name: &str, args: &[Value]) -> Result<Value, String> {
    let arg = |n: usize| args.get(n).cloned().unwrap_or(Value::Null);
    Ok(match name {
        "join" => match arg(0) {
            Value::Array(items) => {
                let separator = match arg(1) {
                    Value::String(text) => unescape_separator(&text),
                    _ => ", ".to_owned(),
                };
                Value::String(
                    items
                        .iter()
                        .map(stringify)
                        .collect::<Vec<_>>()
                        .join(&separator),
                )
            }
            _ => Value::String(String::new()),
        },
        "len" => Value::from(match arg(0) {
            Value::Array(items) => items.len(),
            Value::String(text) => text.encode_utf16().count(),
            _ => 0,
        }),
        "add" => add(&arg(0), &arg(1)),
        "sub" => num_value(to_num(&arg(0)) - to_num(&arg(1))),
        "not" => Value::Bool(!js_truthy(&arg(0))),
        "default" => {
            let value = arg(0);
            if js_truthy(&value) { value } else { arg(1) }
        }
        "pluralize" => {
            let count = arg(0);
            let noun = if count.as_f64() == Some(1.0) {
                arg(1)
            } else {
                arg(2)
            };
            Value::String(format!("{} {}", stringify(&count), stringify(&noun)))
        }
        "includes" => Value::Bool(includes(&arg(0), &arg(1))),
        "lookup" => child(&arg(0), &stringify(&arg(1))),
        "jsonStringify" => Value::String(serde_json::to_string(&arg(0)).unwrap_or_default()),
        "escapeXml" => Value::String(escape_xml(&arg(0))),
        other => return Err(format!("Missing helper: \"{other}\"")),
    })
}

// ---------------------------------------------------------------- rendering

#[derive(Debug)]
struct Frame {
    ctx: Value,
    index: Option<Value>,
    key: Option<Value>,
    first: Option<Value>,
    last: Option<Value>,
}

impl Frame {
    fn new(ctx: Value) -> Self {
        Self {
            ctx,
            index: None,
            key: None,
            first: None,
            last: None,
        }
    }

    fn data(&self, name: &str) -> Option<Value> {
        match name {
            "index" => self.index.clone(),
            "key" => self.key.clone(),
            "first" => self.first.clone(),
            "last" => self.last.clone(),
            _ => None,
        }
    }
}

struct Renderer {
    frames: Vec<Frame>,
}

impl Renderer {
    fn render_nodes(&mut self, nodes: &[Node], out: &mut String) -> Result<(), String> {
        for node in nodes {
            match node {
                Node::Text(text) => out.push_str(text),
                Node::Output(expr) => out.push_str(&stringify(&self.eval(expr)?)),
                Node::Partial(name) => {
                    let Some(partial) = partial_template(name) else {
                        return Err(format!("The partial {name} could not be found"));
                    };
                    self.render_nodes(&partial.nodes, out)?;
                }
                Node::Block {
                    head,
                    inverted,
                    body,
                    otherwise,
                } => {
                    let (main, alt) = if *inverted {
                        (otherwise, body)
                    } else {
                        (body, otherwise)
                    };
                    self.block(head, main, alt, out)?;
                }
            }
        }
        Ok(())
    }

    fn with_frame(&mut self, frame: Frame, nodes: &[Node], out: &mut String) -> Result<(), String> {
        self.frames.push(frame);
        let result = self.render_nodes(nodes, out);
        self.frames.pop();
        result
    }

    fn branch(
        &mut self,
        cond: bool,
        main: &[Node],
        alt: &[Node],
        out: &mut String,
    ) -> Result<(), String> {
        if cond {
            self.render_nodes(main, out)
        } else {
            self.render_nodes(alt, out)
        }
    }

    fn block(
        &mut self,
        head: &Expr,
        main: &[Node],
        alt: &[Node],
        out: &mut String,
    ) -> Result<(), String> {
        if let Expr::Call(call) = head
            && BLOCK_HELPERS.contains(&call.name.as_str())
        {
            return self.block_helper(call, main, alt, out);
        }
        let value = self.eval(head)?;
        self.value_block(value, main, alt, out)
    }

    /// Path block: arrays iterate, empty values take the inverse, anything else sets context.
    fn value_block(
        &mut self,
        value: Value,
        main: &[Node],
        alt: &[Node],
        out: &mut String,
    ) -> Result<(), String> {
        match value {
            Value::Array(items) => self.each_array(&items, main, alt, out),
            other if is_empty(&other) => self.render_nodes(alt, out),
            other => self.with_frame(Frame::new(other), main, out),
        }
    }

    fn each_array(
        &mut self,
        items: &[Value],
        main: &[Node],
        alt: &[Node],
        out: &mut String,
    ) -> Result<(), String> {
        if items.is_empty() {
            return self.render_nodes(alt, out);
        }
        let last = items.len() - 1;
        for (index, item) in items.iter().enumerate() {
            let frame = Frame {
                ctx: item.clone(),
                index: Some(Value::from(index)),
                key: Some(Value::from(index)),
                first: Some(json!(index == 0)),
                last: Some(json!(index == last)),
            };
            self.with_frame(frame, main, out)?;
        }
        Ok(())
    }

    fn block_helper(
        &mut self,
        call: &Call,
        main: &[Node],
        alt: &[Node],
        out: &mut String,
    ) -> Result<(), String> {
        let args = self.eval_args(&call.args)?;
        let hash = self.eval_hash(&call.hash)?;
        let arg = |n: usize| args.get(n).cloned().unwrap_or(Value::Null);
        match call.name.as_str() {
            "if" | "unless" => {
                let include_zero = hash_value(&hash, "includeZero").is_some_and(js_truthy);
                let truthy = if_truthy(&arg(0), include_zero);
                let cond = if call.name == "unless" {
                    !truthy
                } else {
                    truthy
                };
                self.branch(cond, main, alt, out)
            }
            "with" => {
                let value = arg(0);
                if is_empty(&value) {
                    self.render_nodes(alt, out)
                } else {
                    self.with_frame(Frame::new(value), main, out)
                }
            }
            "each" => match arg(0) {
                Value::Array(items) => self.each_array(&items, main, alt, out),
                Value::Object(map) => {
                    if map.is_empty() {
                        return self.render_nodes(alt, out);
                    }
                    for (key, value) in map {
                        let frame = Frame {
                            ctx: value,
                            key: Some(Value::String(key)),
                            ..Frame::new(Value::Null)
                        };
                        self.with_frame(frame, main, out)?;
                    }
                    Ok(())
                }
                _ => self.render_nodes(alt, out),
            },
            "when" => {
                let cond = compare(&arg(0), &stringify(&arg(1)), &arg(2)).unwrap_or(false);
                self.branch(cond, main, alt, out)
            }
            "ifAny" => self.branch(args.iter().any(js_truthy), main, alt, out),
            "ifAll" => self.branch(args.iter().all(js_truthy), main, alt, out),
            "has" => self.branch(collection_has(&arg(0), &arg(1)), main, alt, out),
            "list" => self.list(&arg(0), &hash, main, out),
            "table" => self.table(&arg(0), &hash, main, out),
            "codeblock" => self.codeblock(&hash, main, out),
            "xml" => self.xml(&arg(0), main, out),
            other => Err(format!("Missing helper: \"{other}\"")),
        }
    }

    fn list(
        &mut self,
        items: &Value,
        hash: &[(String, Value)],
        main: &[Node],
        out: &mut String,
    ) -> Result<(), String> {
        let Value::Array(items) = items else {
            return Ok(());
        };
        let prefix = hash_value(hash, "prefix")
            .map(stringify)
            .unwrap_or_default();
        let suffix = hash_value(hash, "suffix")
            .map(stringify)
            .unwrap_or_default();
        let separator = unescape_separator(
            &hash_value(hash, "join")
                .map(stringify)
                .unwrap_or_else(|| "\n".to_owned()),
        );
        let mut parts = Vec::with_capacity(items.len());
        for item in items {
            let mut rendered = String::new();
            self.with_frame(Frame::new(item.clone()), main, &mut rendered)?;
            parts.push(format!("{prefix}{rendered}{suffix}"));
        }
        out.push_str(&parts.join(&separator));
        Ok(())
    }

    /// `{{#table rows headers="A|B"}}…{{/table}}`: a markdown table with one `| row |` line per item.
    fn table(
        &mut self,
        rows: &Value,
        hash: &[(String, Value)],
        main: &[Node],
        out: &mut String,
    ) -> Result<(), String> {
        let Value::Array(rows) = rows else {
            return Ok(());
        };
        if rows.is_empty() {
            return Ok(());
        }
        let headers: Vec<&str> = match hash_value(hash, "headers") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(text)) => text.split('|').collect(),
            Some(_) => return Err("table headers must be a string".to_owned()),
        };
        if !headers.is_empty() {
            let separator = vec!["---"; headers.len()].join(" | ");
            out.push_str(&format!("| {} |\n| {separator} |\n", headers.join(" | ")));
        }
        let mut lines = Vec::with_capacity(rows.len());
        for row in rows {
            let mut rendered = String::new();
            self.with_frame(Frame::new(row.clone()), main, &mut rendered)?;
            lines.push(format!("| {} |", rendered.trim()));
        }
        out.push_str(&lines.join("\n"));
        Ok(())
    }

    /// `{{#codeblock lang="diff"}}…{{/codeblock}}`: the trimmed content inside a fence.
    fn codeblock(
        &mut self,
        hash: &[(String, Value)],
        main: &[Node],
        out: &mut String,
    ) -> Result<(), String> {
        let lang = hash_value(hash, "lang").map(stringify).unwrap_or_default();
        let mut content = String::new();
        self.render_nodes(main, &mut content)?;
        out.push_str(&format!("```{lang}\n{}\n```", content.trim()));
        Ok(())
    }

    /// `{{#xml "tag"}}…{{/xml}}`: wraps trimmed content in `<tag>` lines; empty content renders nothing.
    fn xml(&mut self, tag: &Value, main: &[Node], out: &mut String) -> Result<(), String> {
        let mut content = String::new();
        self.render_nodes(main, &mut content)?;
        let content = content.trim();
        if content.is_empty() {
            return Ok(());
        }
        // JS interpolates a missing tag argument as `undefined`.
        let tag = match tag {
            Value::Null => "undefined".to_owned(),
            other => stringify(other),
        };
        out.push_str(&format!("<{tag}>\n{content}\n</{tag}>"));
        Ok(())
    }

    /// The `arg` helper: `this.args[index - 1]` of the current context; empty when the index is not a
    /// positive integer or the context has no `args` array.
    fn arg_helper(&self, index: &Value) -> Value {
        let parsed = match index {
            Value::Number(number) => number.as_f64(),
            Value::String(text) => js_parse_int(text),
            _ => None,
        };
        let position = parsed
            .map(|number| number - 1.0)
            .filter(|position| position.is_finite() && *position >= 0.0 && position.fract() == 0.0);
        let Some(position) = position else {
            return Value::Null;
        };
        let ctx = self
            .frames
            .last()
            .map_or(Value::Null, |frame| frame.ctx.clone());
        match child(&ctx, "args") {
            Value::Array(items) => items.get(position as usize).cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        }
    }

    fn eval(&self, expr: &Expr) -> Result<Value, String> {
        match expr {
            Expr::Value(arg) => self.eval_arg(arg),
            Expr::Call(call) => {
                if call.name == "arg" {
                    let args = self.eval_args(&call.args)?;
                    Ok(self.arg_helper(&args.into_iter().next().unwrap_or(Value::Null)))
                } else if VALUE_HELPERS.contains(&call.name.as_str()) {
                    let args = self.eval_args(&call.args)?;
                    value_helper(&call.name, &args)
                } else if BLOCK_HELPERS.contains(&call.name.as_str()) {
                    Err(format!("Helper \"{}\" requires a block", call.name))
                } else if call.args.is_empty() && call.hash.is_empty() {
                    Ok(self.lookup(&Path::parse(&call.name)))
                } else {
                    Err(format!("Missing helper: \"{}\"", call.name))
                }
            }
        }
    }

    fn eval_arg(&self, arg: &Arg) -> Result<Value, String> {
        match arg {
            Arg::Lit(value) => Ok(value.clone()),
            Arg::Path(path) => Ok(self.lookup(path)),
            Arg::Sub(expr) => self.eval(expr),
        }
    }

    fn eval_args(&self, args: &[Arg]) -> Result<Vec<Value>, String> {
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.eval_arg(arg)?);
        }
        Ok(values)
    }

    fn eval_hash(&self, hash: &[(String, Arg)]) -> Result<Vec<(String, Value)>, String> {
        let mut values = Vec::with_capacity(hash.len());
        for (key, arg) in hash {
            values.push((key.clone(), self.eval_arg(arg)?));
        }
        Ok(values)
    }

    fn lookup(&self, path: &Path) -> Value {
        let mut value = if let Some(name) = &path.data {
            self.frames
                .iter()
                .rev()
                .find_map(|frame| frame.data(name))
                .unwrap_or(Value::Null)
        } else if path.root {
            self.frames[0].ctx.clone()
        } else {
            self.frames
                .len()
                .checked_sub(path.up + 1)
                .map_or(Value::Null, |index| self.frames[index].ctx.clone())
        };
        for segment in &path.segs {
            value = child(&value, segment);
        }
        value
    }
}

// ---------------------------------------------------------------- post-render format

/// Post-render formatting: trailing whitespace is trimmed outside code fences,
/// markdown table rows are compacted, blank runs collapse to one, blank lines before a closing
/// XML-style tag are dropped, and leading/trailing blank lines are removed.
pub fn format_system(text: &str) -> String {
    let mut lines: Vec<(String, bool)> = Vec::new();
    let mut in_fence = false;
    for line in text.split('\n') {
        let fence = line.trim_start();
        if fence.starts_with("```") || fence.starts_with("~~~") {
            in_fence = !in_fence;
            lines.push((line.to_owned(), true));
        } else if in_fence {
            lines.push((line.to_owned(), true));
        } else {
            let line = line.trim_end();
            let compacted = if is_table_separator(line) {
                compact_separator(line)
            } else if is_table_row(line) {
                compact_row(line)
            } else {
                line.to_owned()
            };
            lines.push((compacted, false));
        }
    }

    let is_blank_plain = |(line, raw): &(String, bool)| !*raw && line.trim().is_empty();
    let mut out: Vec<(String, bool)> = Vec::with_capacity(lines.len());
    for (line, raw) in lines {
        if is_blank_plain(&(line.clone(), raw)) && out.last().is_some_and(is_blank_plain) {
            continue;
        }
        if !raw && is_closing_tag(line.trim()) {
            while out.last().is_some_and(is_blank_plain) {
                out.pop();
            }
        }
        out.push((line, raw));
    }

    let is_content = |(line, raw): &(String, bool)| *raw || !line.trim().is_empty();
    let start = out.iter().position(is_content).unwrap_or(out.len());
    let end = out
        .iter()
        .rposition(is_content)
        .map_or(start, |index| index + 1);
    out[start..end]
        .iter()
        .map(|(line, _)| line.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_table_row(line: &str) -> bool {
    line.len() >= 2 && line.starts_with('|') && line.ends_with('|')
}

fn is_table_separator(line: &str) -> bool {
    if line.len() < 3 || !is_table_row(line) {
        return false;
    }
    let inner = &line[1..line.len() - 1];
    !inner.is_empty()
        && inner
            .chars()
            .all(|c| c == '-' || c == ':' || c == '|' || c.is_whitespace())
}

fn compact_row(line: &str) -> String {
    line.split('|').map(str::trim).collect::<Vec<_>>().join("|")
}

fn compact_separator(line: &str) -> String {
    let cells: Vec<&str> = line
        .split('|')
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .map(|cell| match (cell.starts_with(':'), cell.ends_with(':')) {
            (true, true) => ":---:",
            (true, false) => ":---",
            (false, true) => "---:",
            (false, false) => "---",
        })
        .collect();
    format!("|{}|", cells.join("|"))
}

fn is_closing_tag(line: &str) -> bool {
    let Some(name) = line
        .strip_prefix("</")
        .and_then(|rest| rest.strip_suffix('>'))
    else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '-' || c == '_')
}

// Conventional commit formatting retains its established compact blank-line policy.
fn commit_line_has_content(line: &str) -> bool {
    !line.trim().is_empty()
}

/// Whitespace normalization applied to rendered prompt text: trailing
/// whitespace is removed, blank-line runs are dropped outside fenced code,
/// and leading/trailing blank lines are removed.
pub fn format_commit(content: &str) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut in_code = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code = !in_code;
            out.push(line);
        } else if in_code {
            out.push(line);
        } else if trimmed.is_empty() {
            let next_blank = lines
                .get(i + 1)
                .is_none_or(|next| !commit_line_has_content(next));
            if next_blank {
                while out.last().is_some_and(|l| l.is_empty()) {
                    out.pop();
                }
                let mut j = i + 1;
                while j < lines.len() && !commit_line_has_content(lines[j]) {
                    j += 1;
                }
                i = j;
                continue;
            }
            if out.last().is_some_and(|l| !l.is_empty()) {
                out.push(line);
            }
        } else {
            if trimmed.starts_with("</") && trimmed.ends_with('>') {
                while out.last().is_some_and(|l| l.is_empty()) {
                    out.pop();
                }
            }
            out.push(line);
        }
        i += 1;
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

#[cfg(test)]
mod tests;
