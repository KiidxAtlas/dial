//! Pythonic tool calls, the form Llama 3.2/4 and some Qwen/Granite templates are prompted to write:
//! `[read(path="a.rs"), grep(pattern="x", context=2)]`.
//!
//! Unlike the marker-based forms this has no opener to recognise, so a reply counts only when its
//! whole text is one bracketed list of keyword-argument calls and every callee is an offered tool.
//! Arguments are Python literals (strings, numbers, `True`/`False`/`None`, lists, dicts with string
//! keys); anything else, including positional arguments, rejects the whole reply. Nothing is
//! evaluated.
use serde_json::{Map, Number, Value};

/// The calls in `text` as `(name, arguments)`, or `None` unless `text` is exactly a list of calls to
/// tools in `offered`.
pub(super) fn parse(text: &str, offered: &[String]) -> Option<Vec<(String, Value)>> {
    let mut parser = Parser {
        rest: strip_markers(text.trim()),
    };
    let calls = parser.calls(offered)?;
    parser.skip_space();
    (parser.rest.is_empty() && !calls.is_empty()).then_some(calls)
}

/// Llama 4 wraps the list in `<|python_start|>` / `<|python_end|>`.
fn strip_markers(text: &str) -> &str {
    let text = text.strip_prefix("<|python_start|>").unwrap_or(text);
    text.strip_suffix("<|python_end|>").unwrap_or(text).trim()
}

/// Deepest list or dict nesting accepted in an argument.
const MAX_DEPTH: usize = 16;

struct Parser<'a> {
    rest: &'a str,
}

impl Parser<'_> {
    fn skip_space(&mut self) {
        self.rest = self.rest.trim_start();
    }

    fn eat(&mut self, c: char) -> bool {
        self.skip_space();
        match self.rest.strip_prefix(c) {
            Some(rest) => {
                self.rest = rest;
                true
            }
            None => false,
        }
    }

    fn ident(&mut self) -> Option<&str> {
        self.skip_space();
        let first = self.rest.chars().next()?;
        if !(first.is_ascii_alphabetic() || first == '_') {
            return None;
        }
        let end = self
            .rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(self.rest.len());
        let (ident, rest) = self.rest.split_at(end);
        self.rest = rest;
        Some(ident)
    }

    fn calls(&mut self, offered: &[String]) -> Option<Vec<(String, Value)>> {
        if !self.eat('[') {
            return None;
        }
        let mut calls = Vec::new();
        loop {
            if self.eat(']') {
                return Some(calls);
            }
            let name = self.ident()?.to_owned();
            if !offered.contains(&name) || !self.eat('(') {
                return None;
            }
            let mut arguments = Map::new();
            loop {
                if self.eat(')') {
                    break;
                }
                let key = self.ident()?.to_owned();
                if !self.eat('=') {
                    return None;
                }
                let value = self.value(0)?;
                if arguments.insert(key, value).is_some() {
                    return None;
                }
                if !self.eat(',') {
                    if !self.eat(')') {
                        return None;
                    }
                    break;
                }
            }
            calls.push((name, Value::Object(arguments)));
            if !self.eat(',') {
                return self.eat(']').then_some(calls);
            }
        }
    }

    fn value(&mut self, depth: usize) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.skip_space();
        let first = self.rest.chars().next()?;
        match first {
            '"' | '\'' => self.string().map(Value::String),
            '[' => {
                self.rest = &self.rest[1..];
                self.sequence(']', depth, |parser, depth| parser.value(depth))
                    .map(Value::Array)
            }
            '{' => {
                self.rest = &self.rest[1..];
                let pairs = self.sequence('}', depth, |parser, depth| {
                    parser.skip_space();
                    let key = parser.string()?;
                    parser.eat(':').then_some(())?;
                    Some((key, parser.value(depth)?))
                })?;
                let mut object = Map::new();
                for (key, value) in pairs {
                    // Like a repeated keyword argument, a repeated key is ambiguous: reject.
                    if object.insert(key, value).is_some() {
                        return None;
                    }
                }
                Some(Value::Object(object))
            }
            '-' | '0'..='9' => self.number(),
            _ => match self.ident()? {
                "True" => Some(Value::Bool(true)),
                "False" => Some(Value::Bool(false)),
                "None" => Some(Value::Null),
                _ => None,
            },
        }
    }

    /// Items up to `close`, separated by commas with an optional trailing one.
    fn sequence<T>(
        &mut self,
        close: char,
        depth: usize,
        mut item: impl FnMut(&mut Self, usize) -> Option<T>,
    ) -> Option<Vec<T>> {
        let mut items = Vec::new();
        loop {
            if self.eat(close) {
                return Some(items);
            }
            items.push(item(self, depth + 1)?);
            if !self.eat(',') {
                return self.eat(close).then_some(items);
            }
        }
    }

    fn number(&mut self) -> Option<Value> {
        let end = self
            .rest
            .find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E')))
            .unwrap_or(self.rest.len());
        let (text, rest) = self.rest.split_at(end);
        let number = if let Ok(int) = text.parse::<i64>() {
            Number::from(int)
        } else {
            Number::from_f64(text.parse::<f64>().ok()?)?
        };
        self.rest = rest;
        Some(Value::Number(number))
    }

    /// A single- or double-quoted literal with the usual backslash escapes.
    fn string(&mut self) -> Option<String> {
        let quote = self
            .rest
            .chars()
            .next()
            .filter(|c| matches!(c, '"' | '\''))?;
        let mut out = String::new();
        let mut chars = self.rest[1..].char_indices();
        while let Some((at, c)) = chars.next() {
            match c {
                c if c == quote => {
                    self.rest = &self.rest[at + 2..];
                    return Some(out);
                }
                '\\' => out.push(match chars.next()?.1 {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '\\' => '\\',
                    '\'' => '\'',
                    '"' => '"',
                    // Any other escape is not guessed at.
                    _ => return None,
                }),
                c => out.push(c),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests;
