//! Model-facing names for remote tools: `mcp__<server>__<tool>`, restricted to what function-calling
//! APIs accept (`[A-Za-z0-9_-]`, at most 64 characters) and unique across every connected server.

use std::collections::HashSet;

const MAX_NAME: usize = 64;

/// Replaces every character outside `[A-Za-z0-9_-]` with `_`.
pub fn sanitize(text: &str) -> String {
    text.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// Hands out unique names. Two tools that sanitise or truncate to the same name get `_2`, `_3`, … so
/// no remote tool shadows another.
#[derive(Default)]
pub struct Namer {
    taken: HashSet<String>,
}

impl Namer {
    pub fn name(&mut self, server: &str, tool: &str) -> String {
        let base = truncate(&format!("mcp__{}__{}", sanitize(server), sanitize(tool)), MAX_NAME);
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
mod tests {
    use super::*;

    #[test]
    fn sanitizes_to_the_allowed_alphabet() {
        assert_eq!(sanitize("my server.v2/tool"), "my_server_v2_tool");
        assert_eq!(sanitize("ok-name_1"), "ok-name_1");
        assert_eq!(sanitize("héllo"), "h_llo");
    }

    #[test]
    fn names_are_prefixed_bounded_and_unique() {
        let mut namer = Namer::default();
        assert_eq!(namer.name("fs", "read"), "mcp__fs__read");
        // Same name after sanitising: disambiguated, never replaced.
        assert_eq!(namer.name("fs", "a b"), "mcp__fs__a_b");
        assert_eq!(namer.name("fs", "a.b"), "mcp__fs__a_b_2");
        assert_eq!(namer.name("fs", "a/b"), "mcp__fs__a_b_3");
        // Two servers whose names sanitise alike.
        assert_eq!(namer.name("f.s", "read"), "mcp__f_s__read");
        assert_eq!(namer.name("f s", "read"), "mcp__f_s__read_2");

        let long = "x".repeat(100);
        let first = namer.name("srv", &long);
        let second = namer.name("srv", &long);
        assert_eq!(first.len(), 64);
        assert_eq!(second.len(), 64);
        assert_ne!(first, second);
        assert!(second.ends_with("_2"));
        for name in [first, second] {
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        }
    }
}
