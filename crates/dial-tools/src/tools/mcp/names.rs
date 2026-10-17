//! Model-facing names for remote tools: `mcp__<server>__<tool>`, restricted to what function-calling
//! APIs accept (`[A-Za-z0-9_-]`, at most 64 characters) and unique across every connected server.

use std::collections::HashSet;

const MAX_NAME: usize = 64;

/// Replaces every character outside `[A-Za-z0-9_-]` with `_`.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Hands out unique names. Two tools that sanitise or truncate to the same name get `_2`, `_3`, … so
/// no remote tool shadows another.
#[derive(Default)]
pub struct Namer {
    taken: HashSet<String>,
}

impl Namer {
    pub fn name(&mut self, server: &str, tool: &str) -> String {
        let base = truncate(
            &format!("mcp__{}__{}", sanitize(server), sanitize(tool)),
            MAX_NAME,
        );
        let mut name = base.clone();
        let mut n = 2;
        while !self.taken.insert(name.clone()) {
            let suffix = format!("_{n}");
            name = format!("{}{suffix}", truncate(&base, MAX_NAME - suffix.len()));
            n += 1;
        }
        name
    }
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect() // the input is ASCII after sanitising, so chars are bytes
}

#[cfg(test)]
mod tests;
