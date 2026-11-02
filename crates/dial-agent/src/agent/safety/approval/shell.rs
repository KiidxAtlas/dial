//! Conservative lexical facts shared by approval's distinct shell policy consumers.
//! This is not a shell interpreter: expansions stay nonliteral, and malformed quoting,
//! trailing escapes and line continuations cannot produce a successful scan.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operator {
    Pipe,
    PipeBoth,
    Or,
    And,
    Semicolon,
    Newline,
    Other,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Word { value: String, literal: bool },
    Operator(Operator),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Token {
    pub span: Range<usize>,
    pub kind: Kind,
}

fn operator_char(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '(' | ')' | '{' | '}' | '<' | '>' | '&' | ';' | '|'
    )
}

/// Scan once for quote/escape boundaries, cooked words and raw operator spans.
/// Nonliteral words remain available to risk inspection, but cannot grant approval.
pub(super) fn scan(command: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = command.char_indices().peekable();
    while let Some((start, c)) = chars.peek().copied() {
        if operator_char(c) {
            chars.next();
            let mut end = start + c.len_utf8();
            let next = chars.peek().map(|&(_, c)| c);
            let (operator, paired) = match (c, next) {
                ('|', Some('|')) => (Operator::Or, true),
                ('|', Some('&')) => (Operator::PipeBoth, true),
                ('&', Some('&')) => (Operator::And, true),
                // Case terminators are not plain batch separators.
                (';', Some(';' | '&')) => (Operator::Other, true),
                ('|', _) => (Operator::Pipe, false),
                (';', _) => (Operator::Semicolon, false),
                ('\n', _) => (Operator::Newline, false),
                _ => (Operator::Other, false),
            };
            if paired && let Some((index, c)) = chars.next() {
                end = index + c.len_utf8();
            }
            tokens.push(Token {
                span: start..end,
                kind: Kind::Operator(operator),
            });
            continue;
        }
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut value = String::new();
        let mut literal = true;
        let mut quote = None;
        let mut end = start;
        while let Some((index, c)) = chars.peek().copied() {
            if quote.is_none() && (c.is_whitespace() || operator_char(c)) {
                break;
            }
            chars.next();
            end = index + c.len_utf8();
            match (quote, c) {
                (Some('\''), '\'') | (Some('"'), '"') => quote = None,
                (Some('\''), c) => value.push(c),
                (None, '\'' | '"') => quote = Some(c),
                (_, '\\') => {
                    let (index, escaped) = chars.next()?;
                    if matches!(escaped, '\n' | '\r') {
                        return None;
                    }
                    end = index + escaped.len_utf8();
                    // In double quotes only these escapes remove the backslash.
                    if quote == Some('"') && !matches!(escaped, '$' | '`' | '"' | '\\') {
                        value.push('\\');
                    }
                    value.push(escaped);
                }
                (_, c) => {
                    if matches!(c, '$' | '`')
                        || quote.is_none() && matches!(c, '*' | '?' | '[' | '~')
                    {
                        literal = false;
                    }
                    value.push(c);
                }
            }
        }
        if quote.is_some() {
            return None;
        }
        tokens.push(Token {
            span: start..end,
            kind: Kind::Word { value, literal },
        });
    }
    Some(tokens)
}

/// Preserve source text between selected unquoted operators; consumers select the grammar.
/// Empty segments are retained so a caller cannot accidentally authorize a missing command.
pub(super) fn segments(
    command: &str,
    mut separates: impl FnMut(Operator) -> bool,
) -> Option<Vec<&str>> {
    let mut segments = Vec::new();
    let mut start = 0;
    for token in scan(command)? {
        if let Kind::Operator(operator) = token.kind
            && separates(operator)
        {
            segments.push(&command[start..token.span.start]);
            start = token.span.end;
        }
    }
    segments.push(&command[start..]);
    Some(segments)
}

#[cfg(test)]
mod tests;
